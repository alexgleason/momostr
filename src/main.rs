mod activity;
mod bot;
mod db;
mod error;
mod event_deletion_queue;
mod nostr;
mod nostr_to_ap;
mod rsa_keys;
mod server;
mod util;

use cached::TimedSizedCache;
use db::Db;
use event_deletion_queue::EventDeletionQueue;
use html_to_md::FmtHtmlToMd;
use itertools::Itertools;
use lru::LruCache;
use nostr_lib::{
    EventBuilder, FromBech32, JsonUtil, Kind, Metadata, PublicKey, SecretKey, Timestamp,
};
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use regex::Regex;
use relay_pool::{Filter, RelayPool};
use rustc_hash::{FxHashMap, FxHashSet};
use server::{listen, AppState};
use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

const DOMAIN: &str = env!("DOMAIN");
const REVERSE_DNS: Lazy<String> = Lazy::new(|| DOMAIN.split('.').rev().join("."));
const HTTPS_DOMAIN: &str = env!("HTTPS_DOMAIN");
const NOTE_ID_PREFIX: &str = env!("NOTE_ID_PREFIX");
const USER_ID_PREFIX: &str = env!("USER_ID_PREFIX");
const BIND_ADDRESS: &str = env!("BIND_ADDRESS");
const SECRET_KEY: &str = env!("SECRET_KEY");
static RELAYS: Lazy<Vec<&str>> = Lazy::new(|| {
    env!("MAIN_RELAYS")
        .split(',')
        .filter(|a| !a.is_empty())
        .collect_vec()
});
static INBOX_RELAYS: Lazy<Vec<&str>> = Lazy::new(|| {
    env!("INBOX_RELAYS")
        .split(',')
        .filter(|a| !a.is_empty())
        .collect_vec()
});
static OUTBOX_RELAYS: Lazy<Vec<&str>> = Lazy::new(|| {
    env!("OUTBOX_RELAYS")
        .split(',')
        .filter(|a| !a.is_empty())
        .collect_vec()
});
static METADATA_RELAYS: Lazy<Vec<&str>> = Lazy::new(|| {
    env!("METADATA_RELAYS")
        .split(',')
        .filter(|a| !a.is_empty())
        .collect_vec()
});
static AP_RELAYS: Lazy<Vec<&str>> = Lazy::new(|| {
    env!("AP_RELAYS")
        .split(',')
        .filter(|a| !a.is_empty())
        .collect_vec()
});
const CONTACT_LIST_LEN_LIMIT: usize = 500;
static BOT_SEC: Lazy<SecretKey> = Lazy::new(|| SecretKey::from_bech32(env!("BOT_NSEC")).unwrap());
static BOT_PUB: Lazy<PublicKey> =
    Lazy::new(|| nostr_lib::key::Keys::new(BOT_SEC.clone()).public_key());
static USER_AGENT: Lazy<String> =
    Lazy::new(|| format!("Momostr/{} ({HTTPS_DOMAIN})", env!("CARGO_PKG_VERSION")));
static NPUB_REG: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?:nostr:)?(npub1[0-9a-z]{50,}|nprofile1[0-9a-z]{50,})").unwrap());

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct RelayId(u32);

const MAIN_RELAY: RelayId = RelayId(0);

#[tokio::main]
async fn main() {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,ap=debug,relay_pool=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    assert!(SECRET_KEY.len() > 10);

    let nostr_account_to_followers: FxHashMap<PublicKey, Arc<HashSet<String>>> =
        if let Ok(s) = tokio::fs::read_to_string("nostr_accounts.json").await {
            serde_json::from_str(&s).unwrap()
        } else {
            FxHashMap::default()
        };
    let mut nostr_account_to_followers_rev: FxHashMap<String, FxHashSet<PublicKey>> =
        Default::default();
    for (key, value) in nostr_account_to_followers.iter() {
        for ap in value.iter() {
            nostr_account_to_followers_rev
                .entry(ap.clone())
                .or_default()
                .insert(*key);
        }
    }
    let activitypub_accounts: FxHashMap<PublicKey, Arc<String>> =
        if let Ok(s) = tokio::fs::read_to_string("fediverse_accounts.json").await {
            serde_json::from_str(&s).unwrap()
        } else {
            FxHashMap::default()
        };
    let relays = RELAYS
        .iter()
        .map(|l| url::Url::parse(l).unwrap())
        .collect_vec();
    let nostr = RelayPool::new(USER_AGENT.to_string()).await;
    for (i, l) in relays.iter().enumerate() {
        nostr.add_relay(RelayId(i as u32), l.clone()).await.unwrap();
    }
    let main_relays: Arc<FxHashSet<RelayId>> =
        Arc::new((0..relays.len()).map(|a| RelayId(a as u32)).collect());
    let mut relay_count = RELAYS.len();
    let mut metadata_relays = FxHashSet::default();
    for mr in &*METADATA_RELAYS {
        let i = if let Some(i) = RELAYS.iter().position(|r| r == mr) {
            RelayId(i as u32)
        } else {
            let i = RelayId(relay_count as u32);
            nostr
                .add_relay(i, url::Url::parse(mr).unwrap())
                .await
                .unwrap();
            relay_count += 1;
            i
        };
        metadata_relays.insert(i);
    }
    {
        let key = nostr_lib::Keys::new(BOT_SEC.clone());
        let metadata = EventBuilder::new(
            nostr_lib::Kind::Metadata,
            Metadata {
                name: Some("momostr.pink Bot".to_string()),
                display_name: None,
                about: Some("wip".to_string()),
                website: Some("momostr.pink".to_string()),
                picture: None,
                nip05: None,
                ..Default::default()
            }
            .as_json(),
            [],
        )
        .custom_created_at(Timestamp::from(1700000000))
        .to_event(&key)
        .unwrap();
        nostr.send(Arc::new(metadata), main_relays.clone()).await;
    }
    let filter = get_filter();
    let event_stream = nostr.subscribe(vec![filter], main_relays.clone()).await;
    let http_client = reqwest::Client::new();
    let state = Arc::new(AppState {
        nostr,
        relay_url: relays,
        nostr_account_to_followers: Mutex::new(nostr_account_to_followers),
        nostr_account_to_followers_rev: Mutex::new(nostr_account_to_followers_rev),
        activitypub_accounts: Mutex::new(activitypub_accounts),
        http_client: http_client.clone(),
        note_cache: Mutex::new(LruCache::new(NonZeroUsize::new(1_000).unwrap())),
        actor_cache: Mutex::new(LruCache::new(NonZeroUsize::new(100).unwrap())),
        nostr_user_cache: Mutex::new(TimedSizedCache::with_size_and_lifespan(1_000, 60 * 10)),
        db: Db::new().await,
        main_relays,
        metadata_relays: Arc::new(metadata_relays),
        event_deletion_queue: EventDeletionQueue::new(Arc::new(http_client)),
    });

    tokio::try_join!(
        listen(state.clone()),
        nostr_to_ap::watch(event_stream, &state),
        dead_lock_detection(),
    )
    .unwrap();
}

fn get_filter() -> Filter {
    Filter {
        since: Some(Timestamp::now() - Duration::from_secs(60 * 3)),
        kinds: Some(
            [
                Kind::ContactList,
                Kind::TextNote,
                Kind::EventDeletion,
                Kind::Reaction,
                Kind::Repost,
                Kind::Metadata,
            ]
            .into_iter()
            .collect(),
        ),
        ..Default::default()
    }
}

fn html_to_text(html: &str) -> String {
    FmtHtmlToMd(html).to_string()
}

async fn dead_lock_detection() -> Result<(), error::Error> {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(60 * 2)).await;
        for deadlock in parking_lot::deadlock::check_deadlock() {
            for deadlock in deadlock {
                return Err(error::Error::Internal(
                    anyhow::anyhow!(format!(
                        "found deadlock {}:\n{:?}",
                        deadlock.thread_id(),
                        deadlock.backtrace()
                    ))
                    .into(),
                ));
            }
        }
    }
}
