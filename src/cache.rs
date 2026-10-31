//! Valkey-backed shared state: rate limits, cooldowns, spend counters, cluster sync.
//!
//! Every replica talks to the same Valkey, so limits are cluster-wide.
//! Valkey outages fail open (requests are served, limits are not enforced).
//!
//! Valkey is either a single primary or a cluster (`VALKEY_CLUSTER`, which ElastiCache Serverless
//! also needs). Every command, script and pipeline here touches keys of one hash slot only, and
//! nothing lists keys with SCAN: what has to be enumerated (replicas, conversations) is kept in
//! an index of its own.

use std::{collections::HashMap, sync::LazyLock, time::Duration};

use futures::{StreamExt, future::join_all, stream::BoxStream};
use redis::{
    AsyncCommands, Client, Cmd, Msg, Pipeline, PushKind, RedisFuture, RedisResult, Script, Value,
    aio::ConnectionManager,
    cluster::{ClusterClientBuilder, ClusterConfig},
    cluster_async::ClusterConnection,
};
use uuid::Uuid;

use crate::budget::{BudgetPeriod, Spender};

pub const SYNC_CHANNEL: &str = "gw:sync";

const CONNECTION_TIMEOUT: Duration = Duration::from_secs(2);
const RESPONSE_TIMEOUT: Duration = Duration::from_millis(1500);

/// Replicas' heartbeats (field = instance id) and when each one lapses (sorted set, score = Unix
/// seconds). The hash tag keeps both in one slot.
const INSTANCES_KEY: &str = "gw:{inst}:info";
const INSTANCES_SEEN_KEY: &str = "gw:{inst}:seen";

/// Every stored conversation and when it expires (sorted set, member = root, score = Unix
/// seconds), so key rotation can find them.
const CONVERSATIONS_KEY: &str = "gw:convidx";

/// Sliding-window limiter over two fixed 60s windows. Hash tags keep both keys in one
/// cluster slot. With `incr == 0` it only checks that the window is not exhausted.
static RATE_SCRIPT: LazyLock<Script> = LazyLock::new(|| {
    Script::new(
        r"
local cur = tonumber(redis.call('GET', KEYS[1]) or '0')
local prev = tonumber(redis.call('GET', KEYS[2]) or '0')
local limit = tonumber(ARGV[1])
local incr = tonumber(ARGV[3])
local est = math.floor(prev * tonumber(ARGV[2]) / 1000) + cur
if (incr > 0 and est + incr > limit) or (incr == 0 and est >= limit) then
  return {0, est}
end
if incr > 0 then
  redis.call('INCRBY', KEYS[1], incr)
  redis.call('EXPIRE', KEYS[1], 130)
end
return {1, est}
",
    )
});

/// Conversation storage quota. KEYS[1] holds the size of each of a virtual key's conversations
/// (hash, field = root), KEYS[2] when each expires (sorted set, score = Unix seconds). Drops the
/// conversations expired by ARGV[4] (now), then adds ARGV[2] bytes to conversation ARGV[1] if
/// the live ones' total stays within ARGV[3], and extends it for ARGV[5] seconds. Both keys
/// live as long as the conversation that expires last.
static CONVERSATION_QUOTA_SCRIPT: LazyLock<Script> = LazyLock::new(|| {
    Script::new(
        r"
local now, ttl = tonumber(ARGV[4]), tonumber(ARGV[5])
for _, root in ipairs(redis.call('ZRANGEBYSCORE', KEYS[2], '-inf', now)) do redis.call('HDEL', KEYS[1], root) end
redis.call('ZREMRANGEBYSCORE', KEYS[2], '-inf', now)
local used = 0
for _, v in ipairs(redis.call('HVALS', KEYS[1])) do used = used + tonumber(v) end
local n = tonumber(ARGV[2])
if used + n > tonumber(ARGV[3]) then return 0 end
redis.call('HINCRBY', KEYS[1], ARGV[1], n)
redis.call('ZADD', KEYS[2], now + ttl, ARGV[1])
for _, k in ipairs(KEYS) do
  if redis.call('TTL', k) < ttl then redis.call('EXPIRE', k, ttl) end
end
return 1
",
    )
});

/// Increment the existing ones among KEYS by ARGV[1].
static SPEND_SCRIPT: LazyLock<Script> = LazyLock::new(|| {
    Script::new(
        r"
for _, k in ipairs(KEYS) do
  if redis.call('EXISTS', k) == 1 then redis.call('INCRBYFLOAT', k, ARGV[1]) end
end
return 0
",
    )
});

/// Heartbeat of replica ARGV[1] with info ARGV[2], valid for ARGV[3] seconds. Drops replicas
/// whose heartbeat lapsed; both keys go away once no replica sends one. Uses the server's clock,
/// so replicas need not agree on the time.
static HEARTBEAT_SCRIPT: LazyLock<Script> = LazyLock::new(|| {
    Script::new(
        r"
local now = tonumber(redis.call('TIME')[1])
local ttl = tonumber(ARGV[3])
local gone = redis.call('ZRANGEBYSCORE', KEYS[2], '-inf', now)
for _, id in ipairs(gone) do redis.call('HDEL', KEYS[1], id) end
redis.call('ZREMRANGEBYSCORE', KEYS[2], '-inf', now)
redis.call('HSET', KEYS[1], ARGV[1], ARGV[2])
redis.call('ZADD', KEYS[2], now + ttl, ARGV[1])
redis.call('EXPIRE', KEYS[1], ttl)
redis.call('EXPIRE', KEYS[2], ttl)
return 0
",
    )
});

/// Info of the replicas whose heartbeat has not lapsed.
static INSTANCES_SCRIPT: LazyLock<Script> = LazyLock::new(|| {
    Script::new(
        r"
local now = tonumber(redis.call('TIME')[1])
local live = redis.call('ZRANGEBYSCORE', KEYS[2], '(' .. now, '+inf')
if #live == 0 then return {} end
return redis.call('HMGET', KEYS[1], unpack(live))
",
    )
});

/// A deployment's recent health (KEYS[1], a hash): successes and failures that halve every ARGV[5]
/// ms, and an average time to first token. Decays what is stored to now (ARGV[1], Unix ms), then
/// adds ARGV[2] successes, ARGV[3] failures and, if ARGV[4] >= 0, a time to first token in ms.
static HEALTH_SCRIPT: LazyLock<Script> = LazyLock::new(|| {
    Script::new(
        r"
local h = redis.call('HMGET', KEYS[1], 'ok', 'err', 'ttft', 'ts')
local now = tonumber(ARGV[1])
local ts = tonumber(h[4] or ARGV[1])
local decay = 0.5 ^ (math.max(0, now - ts) / tonumber(ARGV[5]))
local ok = tonumber(h[1] or '0') * decay + tonumber(ARGV[2])
local err = tonumber(h[2] or '0') * decay + tonumber(ARGV[3])
local ttft = tonumber(h[3] or '-1')
local sample = tonumber(ARGV[4])
if sample >= 0 then
  if ttft < 0 then ttft = sample else ttft = ttft * 0.8 + sample * 0.2 end
end
redis.call('HSET', KEYS[1], 'ok', tostring(ok), 'err', tostring(err), 'ttft', tostring(ttft), 'ts', tostring(now))
redis.call('PEXPIRE', KEYS[1], 600000)
return 0
",
    )
});

/// Successes and failures in a deployment's health count half as much after this long.
pub const HEALTH_HALF_LIFE_MS: i64 = 60_000;

/// A deployment's recent health, decayed to when it was read (see `HEALTH_SCRIPT`).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Health {
    pub ok: f64,
    pub err: f64,
    /// Average time to first token of streamed responses, in ms.
    pub ttft_ms: Option<f64>,
}

/// What happened when a deployment was called, for its health.
pub enum Outcome {
    Succeeded { ttft_ms: Option<u64> },
    Failed,
}

/// A connection to a single Valkey or to a cluster. Both reconnect on their own.
#[derive(Clone)]
pub enum Conn {
    Single(ConnectionManager),
    Cluster(ClusterConnection),
}

impl redis::aio::ConnectionLike for Conn {
    fn req_packed_command<'a>(&'a mut self, cmd: &'a Cmd) -> RedisFuture<'a, Value> {
        match self {
            Conn::Single(c) => c.req_packed_command(cmd),
            Conn::Cluster(c) => c.req_packed_command(cmd),
        }
    }

    fn req_packed_commands<'a>(&'a mut self, pipeline: &'a Pipeline, offset: usize, count: usize) -> RedisFuture<'a, Vec<Value>> {
        match self {
            Conn::Single(c) => c.req_packed_commands(pipeline, offset, count),
            Conn::Cluster(c) => c.req_packed_commands(pipeline, offset, count),
        }
    }

    fn get_db(&self) -> i64 {
        match self {
            Conn::Single(c) => c.get_db(),
            Conn::Cluster(c) => c.get_db(),
        }
    }
}

/// Where subscriptions connect to: they need a connection of their own.
#[derive(Clone)]
enum Target {
    Single(Client),
    Cluster(Vec<String>),
}

#[derive(Clone)]
pub struct Cache {
    target: Target,
    conn: Conn,
}

pub struct RateDecision {
    pub allowed: bool,
    pub retry_after: u64,
}

/// A cluster client for the comma-separated seed nodes in `nodes`; it learns the rest itself.
fn cluster_builder(nodes: &[String]) -> ClusterClientBuilder {
    ClusterClientBuilder::new(nodes.to_vec())
        .connection_timeout(CONNECTION_TIMEOUT)
        .response_timeout(RESPONSE_TIMEOUT)
        .retries(2)
}

impl Cache {
    /// `url` is one Valkey, or with `cluster` the comma-separated seed nodes of a cluster. No
    /// connection is made yet.
    pub fn new(url: &str, cluster: bool) -> RedisResult<Self> {
        if cluster {
            let nodes: Vec<String> = url.split(',').map(str::trim).filter(|n| !n.is_empty()).map(String::from).collect();
            let config = ClusterConfig::new()
                .set_connection_timeout(CONNECTION_TIMEOUT)
                .set_response_timeout(RESPONSE_TIMEOUT);
            let conn = cluster_builder(&nodes).build()?.get_pending_async_connection_with_config(config);
            return Ok(Self { target: Target::Cluster(nodes), conn: Conn::Cluster(conn) });
        }
        let client = Client::open(url)?;
        let config = redis::aio::ConnectionManagerConfig::new()
            .set_connection_timeout(Some(CONNECTION_TIMEOUT))
            .set_response_timeout(Some(RESPONSE_TIMEOUT))
            .set_number_of_retries(2)
            .set_max_delay(Duration::from_secs(2));
        let conn = ConnectionManager::new_lazy_with_config(client.clone(), config)?;
        Ok(Self { target: Target::Single(client), conn: Conn::Single(conn) })
    }

    fn conn(&self) -> Conn {
        self.conn.clone()
    }

    /// A handle on the shared connection, for stores that keep their own keys (admin sessions).
    pub fn connection(&self) -> Conn {
        self.conn.clone()
    }

    /// Whether Valkey answers. A cluster must also consider itself healthy: a PING reaches only
    /// one node, and the client skips nodes it lost when sending to all of them.
    pub async fn ping(&self) -> bool {
        match self.conn() {
            mut conn @ Conn::Cluster(_) => redis::cmd("CLUSTER")
                .arg("INFO")
                .query_async::<String>(&mut conn)
                .await
                .is_ok_and(|info| info.lines().any(|l| l.trim() == "cluster_state:ok")),
            mut conn => redis::cmd("PING").query_async::<String>(&mut conn).await.is_ok(),
        }
    }

    /// Messages published on `channel`, over a connection of their own. The stream ends when that
    /// connection drops; messages sent in the meantime are lost.
    pub async fn subscribe(&self, channel: &'static str) -> RedisResult<BoxStream<'static, Msg>> {
        match &self.target {
            Target::Single(client) => {
                let mut pubsub = client.get_async_pubsub().await?;
                pubsub.subscribe(channel).await?;
                Ok(pubsub.into_on_message().boxed())
            }
            Target::Cluster(nodes) => {
                // Cluster subscriptions arrive as RESP3 pushes.
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                let client = cluster_builder(nodes).use_protocol(redis::ProtocolVersion::RESP3).push_sender(tx).build()?;
                let mut conn = client.get_async_connection().await?;
                conn.subscribe(channel).await?;
                // The stream keeps the connection open.
                Ok(futures::stream::unfold((rx, conn), |(mut rx, conn)| async move {
                    loop {
                        let push = rx.recv().await?;
                        if push.kind == PushKind::Disconnection {
                            return None;
                        }
                        if let Some(msg) = Msg::from_push_info(push) {
                            return Some((msg, (rx, conn)));
                        }
                    }
                })
                .boxed())
            }
        }
    }

    /// Check (and with `incr > 0`, consume) a per-minute budget for `scope`.
    pub async fn rate_check(&self, scope: &str, limit: u64, incr: u64) -> RateDecision {
        let now_ms = crate::util::now_ms();
        let window = now_ms / 60_000;
        let elapsed_permille = (now_ms % 60_000) / 60;
        let prev_weight = 1000 - elapsed_permille;
        let cur_key = format!("gw:rl:{{{scope}}}:{window}");
        let prev_key = format!("gw:rl:{{{scope}}}:{}", window - 1);
        let res: RedisResult<(i64, i64)> = RATE_SCRIPT
            .key(cur_key)
            .key(prev_key)
            .arg(limit)
            .arg(prev_weight)
            .arg(incr)
            .invoke_async(&mut self.conn())
            .await;
        match res {
            Ok((allowed, _)) => RateDecision {
                allowed: allowed == 1,
                retry_after: (60 - (now_ms % 60_000) / 1000).max(1) as u64,
            },
            Err(e) => {
                tracing::warn!(error = %e, "valkey rate check failed; allowing request");
                RateDecision { allowed: true, retry_after: 0 }
            }
        }
    }

    /// Record consumption after the fact (token counts are known only at the end).
    async fn rate_add(&self, scope: &str, amount: u64) {
        let window = crate::util::now_ms() / 60_000;
        let key = format!("gw:rl:{{{scope}}}:{window}");
        let r: RedisResult<()> = redis::pipe()
            .incr(&key, amount)
            .ignore()
            .expire(&key, 130)
            .ignore()
            .query_async(&mut self.conn())
            .await;
        if let Err(e) = r {
            tracing::warn!(error = %e, "failed to record token usage in valkey");
        }
    }

    pub async fn set_cooldown(&self, deployment: Uuid, dur: Duration) {
        let r: RedisResult<()> = self
            .conn()
            .set_ex(format!("gw:cd:{deployment}"), 1, dur.as_secs().max(1))
            .await;
        if let Err(e) = r {
            tracing::warn!(error = %e, "failed to set cooldown");
        }
    }

    /// Which of the given deployments are currently cooling down. One command per deployment:
    /// their keys are in different slots.
    pub async fn cooldowns(&self, ids: &[Uuid]) -> Vec<bool> {
        join_all(ids.iter().map(|id| async move {
            self.conn().exists::<_, bool>(format!("gw:cd:{id}")).await.unwrap_or(false)
        }))
        .await
    }

    pub async fn cooldown_ttls(&self, ids: &[Uuid]) -> HashMap<Uuid, i64> {
        let ttls = join_all(ids.iter().map(|id| async move {
            self.conn().ttl::<_, i64>(format!("gw:cd:{id}")).await.unwrap_or(-2)
        }))
        .await;
        ids.iter().copied().zip(ttls).filter(|(_, ttl)| *ttl > 0).collect()
    }

    pub async fn clear_cooldown(&self, id: Uuid) {
        let _: RedisResult<()> = self.conn().del(format!("gw:cd:{id}")).await;
    }

    /// Count a call to a deployment in its health (see `router`).
    pub async fn record_health(&self, deployment: Uuid, outcome: Outcome) {
        let (ok, err, ttft) = match outcome {
            Outcome::Succeeded { ttft_ms } => (1, 0, ttft_ms.map_or(-1, |t| t as i64)),
            Outcome::Failed => (0, 1, -1),
        };
        let r: RedisResult<()> = HEALTH_SCRIPT
            .key(format!("gw:health:{deployment}"))
            .arg(crate::util::now_ms())
            .arg(ok)
            .arg(err)
            .arg(ttft)
            .arg(HEALTH_HALF_LIFE_MS)
            .invoke_async(&mut self.conn())
            .await;
        if let Err(e) = r {
            tracing::warn!(error = %e, "failed to record deployment health");
        }
    }

    /// The health of the given deployments, decayed to now; unknown (or unreadable) is empty.
    /// One command per deployment: their keys are in different slots.
    pub async fn health(&self, ids: &[Uuid]) -> Vec<Health> {
        let now = crate::util::now_ms();
        join_all(ids.iter().map(|id| async move {
            let fields: RedisResult<(Option<f64>, Option<f64>, Option<f64>, Option<i64>)> =
                redis::cmd("HMGET").arg(format!("gw:health:{id}")).arg(&["ok", "err", "ttft", "ts"]).query_async(&mut self.conn()).await;
            match fields {
                Ok((Some(ok), Some(err), ttft, Some(ts))) => {
                    let decay = 0.5f64.powf((now - ts).max(0) as f64 / HEALTH_HALF_LIFE_MS as f64);
                    Health { ok: ok * decay, err: err * decay, ttft_ms: ttft.filter(|t| *t >= 0.0) }
                }
                _ => Health::default(),
            }
        }))
        .await
    }

    /// The deployment (or for an auto router, the route) a conversation went to, by its affinity
    /// key; reading it keeps it for another `ttl`.
    pub async fn affinity(&self, key: &str, ttl: Duration) -> Option<String> {
        let r: RedisResult<Option<String>> =
            redis::cmd("GETEX").arg(key).arg("EX").arg(ttl.as_secs().max(1)).query_async(&mut self.conn()).await;
        r.unwrap_or_else(|e| {
            tracing::warn!(error = %e, "failed to read routing affinity");
            None
        })
    }

    pub async fn set_affinity(&self, key: &str, value: &str, ttl: Duration) {
        let r: RedisResult<()> = self.conn().set_ex(key, value, ttl.as_secs().max(1)).await;
        if let Err(e) = r {
            tracing::warn!(error = %e, "failed to save routing affinity");
        }
    }

    /// The key that an A2A task or context belongs to (see `a2a`); reading it keeps it for another
    /// `ttl`. `None` when unknown, or when Valkey cannot be reached.
    pub async fn a2a_owner(&self, key: &str, ttl: Duration) -> Option<String> {
        let r: RedisResult<Option<String>> =
            redis::cmd("GETEX").arg(key).arg("EX").arg(ttl.as_secs().max(1)).query_async(&mut self.conn()).await;
        r.unwrap_or_else(|e| {
            tracing::warn!(error = %e, "failed to read an A2A task's owner");
            None
        })
    }

    /// Records the key an A2A task or context belongs to, unless another key holds it already.
    pub async fn set_a2a_owner(&self, key: &str, owner: &str, ttl: Duration) {
        let r: RedisResult<()> = redis::cmd("SET").arg(key).arg(owner).arg("NX").arg("EX").arg(ttl.as_secs().max(1)).query_async(&mut self.conn()).await;
        if let Err(e) = r {
            tracing::warn!(error = %e, "failed to save an A2A task's owner");
        }
    }

    /// Per-period spend counter of a key or a project. The hash tag keeps one spender's counters
    /// in one cluster slot.
    fn spend_key(who: Spender, window_id: &str) -> String {
        match who {
            Spender::Key(id) => format!("gw:spend:{{{id}}}:{window_id}"),
            Spender::Project(id) => format!("gw:pspend:{{{id}}}:{window_id}"),
        }
    }

    /// Spend in one budget period, or `None` if the counter does not exist (yet).
    pub async fn period_spend(&self, who: Spender, window_id: &str) -> Option<f64> {
        self.conn().get::<_, Option<f64>>(Self::spend_key(who, window_id)).await.ok().flatten()
    }

    /// Create a period counter from Postgres. NX: another replica may have seeded it already.
    pub async fn seed_period_spend(&self, who: Spender, window_id: &str, spent: f64, ttl_secs: u64) {
        let _: RedisResult<()> = redis::cmd("SET")
            .arg(Self::spend_key(who, window_id))
            .arg(spent)
            .arg("NX")
            .arg("EX")
            .arg(ttl_secs)
            .query_async(&mut self.conn())
            .await;
    }

    /// Drop current period counters; they are rebuilt from Postgres on next use.
    pub async fn reset_period_spend(&self, who: Spender) {
        let now = chrono::Utc::now();
        let keys: Vec<String> = BudgetPeriod::ALL.iter().map(|p| Self::spend_key(who, &p.window(now).id)).collect();
        let _: RedisResult<()> = self.conn().del(keys).await;
    }

    pub async fn publish(&self, msg: &str) {
        let r: RedisResult<i64> = self.conn().publish(SYNC_CHANNEL, msg).await;
        if let Err(e) = r {
            tracing::warn!(error = %e, "failed to publish sync message; peers will catch up on periodic reload");
        }
    }

    pub async fn heartbeat(&self, instance_id: &str, info: &str, ttl: u64) {
        let r: RedisResult<()> = HEARTBEAT_SCRIPT
            .key(INSTANCES_KEY)
            .key(INSTANCES_SEEN_KEY)
            .arg(instance_id)
            .arg(info)
            .arg(ttl)
            .invoke_async(&mut self.conn())
            .await;
        if let Err(e) = r {
            tracing::warn!(error = %e, "failed to send heartbeat");
        }
    }

    pub async fn instances(&self) -> Vec<serde_json::Value> {
        let vals: Vec<Option<String>> = INSTANCES_SCRIPT
            .key(INSTANCES_KEY)
            .key(INSTANCES_SEEN_KEY)
            .invoke_async(&mut self.conn())
            .await
            .unwrap_or_default();
        vals.into_iter().flatten().filter_map(|v| serde_json::from_str(&v).ok()).collect()
    }

    /// Add a finished request's tokens and spend to the shared limit counters. Metrics come
    /// from the request log in Postgres, not from here.
    pub async fn record(&self, rec: &crate::accounting::RequestRecord) {
        let tokens = rec.usage.total();
        // The calls of a routed request count against the key through the router's entry.
        let key_id = rec.key_id.filter(|_| rec.parent_id.is_none());
        let key_tokens = async {
            if let Some(k) = key_id
                && tokens > 0
            {
                self.rate_add(&format!("k:{k}:tpm"), tokens).await;
            }
        };
        let deployment_tokens = async {
            if let Some(d) = rec.deployment_id
                && tokens > 0
            {
                self.rate_add(&format!("d:{d}:tpm"), tokens).await;
            }
        };
        // Add spend to the key's and its project's day/week/month counters, but only to counters
        // that exist: a missing one is (re)built from Postgres on its next budget check, which
        // already includes this request once it is logged.
        let spend = |who: Option<Spender>| async move {
            if let Some(who) = who
                && rec.cost_usd > 0.0
            {
                let mut inv = SPEND_SCRIPT.prepare_invoke();
                for p in BudgetPeriod::ALL {
                    inv.key(Self::spend_key(who, &p.window(rec.ts).id));
                }
                if let Err(e) = inv.arg(rec.cost_usd).invoke_async::<()>(&mut self.conn()).await {
                    tracing::warn!(error = %e, "failed to record spend in valkey");
                }
            }
        };
        let project_id = rec.project_id.filter(|_| rec.parent_id.is_none());
        tokio::join!(key_tokens, deployment_tokens, spend(key_id.map(Spender::Key)), spend(project_id.map(Spender::Project)));
    }

    /// All stored turns of one conversation (field = response id, value = sealed entry).
    pub async fn conversation(&self, root: &str) -> RedisResult<HashMap<String, Vec<u8>>> {
        self.conn().hgetall(format!("gw:conv:{{{root}}}")).await
    }

    /// Extend a conversation's lifetime, in the conversation index and in its key's quota too,
    /// so it cannot expire while a continuation is being answered.
    pub async fn touch_conversation(&self, root: &str, key_id: Uuid, ttl: Duration) -> RedisResult<()> {
        let key = format!("gw:conv:{{{root}}}");
        let (secs, until) = (ttl.as_secs() as i64, crate::util::now_ms() / 1000 + ttl.as_secs() as i64);
        let [sizes, expiries] = Self::conversation_quota_keys(key_id);
        let mut quota = redis::pipe();
        quota
            .cmd("ZADD")
            .arg(&expiries)
            .arg("XX")
            .arg(until)
            .arg(root)
            .ignore()
            .expire(&sizes, secs)
            .ignore()
            .expire(&expiries, secs)
            .ignore();
        let (mut c1, mut c2, mut c3) = (self.conn(), self.conn(), self.conn());
        tokio::try_join!(
            c1.expire::<_, ()>(&key, secs),
            c2.zadd::<_, _, _, ()>(CONVERSATIONS_KEY, root, until),
            quota.query_async::<()>(&mut c3),
        )
        .map(|_| ())
    }

    /// A virtual key's conversation sizes and expiry times. The hash tag keeps them in one slot.
    fn conversation_quota_keys(key_id: Uuid) -> [String; 2] {
        [format!("gw:convq:{{{key_id}}}:size"), format!("gw:convq:{{{key_id}}}:exp")]
    }

    /// Count `bytes` more of conversation `root` against its virtual key's quota: the key's
    /// conversations that have not expired may not exceed `quota` together. Returns whether the
    /// bytes fit; they are counted, and the conversation extended by `ttl`, only if they do.
    pub async fn reserve_conversation_bytes(&self, key_id: Uuid, root: &str, bytes: u64, quota: u64, ttl: Duration) -> RedisResult<bool> {
        let now = crate::util::now_ms() / 1000;
        let ok: i64 = CONVERSATION_QUOTA_SCRIPT
            .key(&Self::conversation_quota_keys(key_id))
            .arg(root)
            .arg(bytes)
            .arg(quota)
            .arg(now)
            .arg(ttl.as_secs())
            .invoke_async(&mut self.conn())
            .await?;
        Ok(ok == 1)
    }

    /// Store one turn and extend the whole conversation's lifetime, in the conversation index too
    /// (which drops conversations that expired meanwhile).
    pub async fn save_turn(&self, root: &str, response_id: &str, sealed: &[u8], ttl: Duration) -> RedisResult<()> {
        let key = format!("gw:conv:{{{root}}}");
        let now = crate::util::now_ms() / 1000;
        let mut turn = redis::pipe();
        turn.hset(&key, response_id, sealed).ignore().expire(&key, ttl.as_secs() as i64).ignore();
        let mut index = redis::pipe();
        index
            .zadd(CONVERSATIONS_KEY, root, now + ttl.as_secs() as i64)
            .ignore()
            .zrembyscore(CONVERSATIONS_KEY, "-inf", now)
            .ignore();
        let (mut c1, mut c2) = (self.conn(), self.conn());
        tokio::try_join!(turn.query_async::<()>(&mut c1), index.query_async::<()>(&mut c2)).map(|_| ())
    }

    /// Remember which deployment holds an upstream-stored response (native Responses passthrough).
    pub async fn save_pin(&self, response_id: &str, value: &str, ttl: Duration) -> RedisResult<()> {
        self.conn().set_ex(format!("gw:pin:{response_id}"), value, ttl.as_secs()).await
    }

    /// The deployment holding an upstream-stored response; reading it extends its lifetime.
    pub async fn pin(&self, response_id: &str, ttl: Duration) -> RedisResult<Option<String>> {
        redis::cmd("GETEX").arg(format!("gw:pin:{response_id}")).arg("EX").arg(ttl.as_secs()).query_async(&mut self.conn()).await
    }

    // The key rotation command works on whole conversations by key.

    /// The keys of the conversations that have not expired, from the conversation index.
    pub async fn conversation_keys(&self) -> RedisResult<Vec<String>> {
        let now = crate::util::now_ms() / 1000;
        let roots: Vec<String> = self.conn().zrangebyscore(CONVERSATIONS_KEY, format!("({now}"), "+inf").await?;
        Ok(roots.into_iter().map(|root| format!("gw:conv:{{{root}}}")).collect())
    }

    pub async fn conversation_turns(&self, key: &str) -> RedisResult<HashMap<String, Vec<u8>>> {
        self.conn().hgetall(key).await
    }

    /// Rewrite turns in place, keeping the conversation's remaining lifetime.
    pub async fn replace_turns(&self, key: &str, turns: &[(String, Vec<u8>)]) -> RedisResult<()> {
        let mut conn = self.conn();
        let ttl: i64 = conn.ttl(key).await?;
        let mut pipe = redis::pipe();
        pipe.hset_multiple(key, turns).ignore();
        if ttl > 0 {
            pipe.expire(key, ttl).ignore();
        }
        pipe.query_async(&mut conn).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs a Valkey: `GATEWAY_TEST_VALKEY_URL=redis://127.0.0.1:6379 cargo test`, plus
    /// `GATEWAY_TEST_VALKEY_CLUSTER=true` when that is a cluster. Skipped otherwise.
    fn test_cache() -> Option<Cache> {
        let url = std::env::var("GATEWAY_TEST_VALKEY_URL").ok()?;
        let cluster = std::env::var("GATEWAY_TEST_VALKEY_CLUSTER").is_ok_and(|v| v == "true");
        Some(Cache::new(&url, cluster).unwrap())
    }

    /// Uses a random key id and removes its counters afterwards.
    #[tokio::test]
    async fn conversation_quota_counts_live_conversations() {
        let Some(cache) = test_cache() else { return };
        let key = Uuid::new_v4();
        let (long, short) = (Duration::from_secs(600), Duration::from_secs(1));
        let reserve = |root, n, ttl| cache.reserve_conversation_bytes(key, root, n, 100, ttl);
        assert!(reserve("a", 40, long).await.unwrap());
        assert!(reserve("b", 30, short).await.unwrap());
        assert!(!reserve("a", 40, long).await.unwrap(), "40 + 30 + 40 is over the quota");
        assert!(reserve("a", 30, long).await.unwrap(), "a refused write is not counted");
        assert!(!reserve("c", 1, long).await.unwrap());
        // Another key has its own quota.
        let other = Uuid::new_v4();
        assert!(cache.reserve_conversation_bytes(other, "a", 100, 100, long).await.unwrap());
        // An expired conversation no longer counts; one being continued does.
        tokio::time::sleep(Duration::from_millis(2100)).await;
        assert!(reserve("c", 30, long).await.unwrap(), "b expired");
        assert!(!reserve("c", 1, long).await.unwrap());
        let [sizes, expiries] = Cache::conversation_quota_keys(key);
        let roots: Vec<String> = cache.conn().zrange(&expiries, 0, -1).await.unwrap();
        assert_eq!(roots, ["a", "c"]);
        cache.touch_conversation("a", key, Duration::from_secs(3600)).await.unwrap();
        let left: i64 = cache.conn().ttl(&sizes).await.unwrap();
        assert!(left > 600 && left <= 3600, "{left}");
        let until: f64 = cache.conn().zscore(&expiries, "a").await.unwrap();
        assert!(until as i64 > crate::util::now_ms() / 1000 + 600);
        let _: () = cache.conn().del(&[sizes, expiries]).await.unwrap();
        let _: () = cache.conn().del(&Cache::conversation_quota_keys(other)).await.unwrap();
        let _: () = cache.conn().zrem(CONVERSATIONS_KEY, "a").await.unwrap();
    }

    #[tokio::test]
    async fn cooldowns_span_slots() {
        let Some(cache) = test_cache() else { return };
        let ids: Vec<Uuid> = (0..8).map(|_| Uuid::new_v4()).collect();
        for id in ids.iter().step_by(2) {
            cache.set_cooldown(*id, Duration::from_secs(30)).await;
        }
        let expected: Vec<bool> = (0..8).map(|i| i % 2 == 0).collect();
        assert_eq!(cache.cooldowns(&ids).await, expected);
        assert_eq!(cache.cooldown_ttls(&ids).await.len(), 4);
        for id in &ids {
            cache.clear_cooldown(*id).await;
        }
        assert_eq!(cache.cooldowns(&ids).await, vec![false; 8]);
    }

    #[tokio::test]
    async fn health_counts_calls_and_averages_ttft() {
        let Some(cache) = test_cache() else { return };
        let (id, unknown) = (Uuid::new_v4(), Uuid::new_v4());
        for ttft in [100, 200] {
            cache.record_health(id, Outcome::Succeeded { ttft_ms: Some(ttft) }).await;
        }
        cache.record_health(id, Outcome::Succeeded { ttft_ms: None }).await;
        cache.record_health(id, Outcome::Failed).await;
        let h = cache.health(&[id, unknown]).await;
        // Calls a moment apart have barely decayed.
        assert!((h[0].ok - 3.0).abs() < 0.01 && (h[0].err - 1.0).abs() < 0.01, "{:?}", h[0]);
        assert!((h[0].ttft_ms.unwrap() - 120.0).abs() < 0.01, "100, then 0.8 x 100 + 0.2 x 200");
        assert_eq!(h[1], Health::default());
        let _: () = cache.conn().del(format!("gw:health:{id}")).await.unwrap();
    }

    #[tokio::test]
    async fn affinities_are_kept_while_read() {
        let Some(cache) = test_cache() else { return };
        let key = format!("gw:aff:test-{}", Uuid::new_v4());
        assert_eq!(cache.affinity(&key, Duration::from_secs(60)).await, None);
        cache.set_affinity(&key, "d1", Duration::from_secs(5)).await;
        assert_eq!(cache.affinity(&key, Duration::from_secs(60)).await.as_deref(), Some("d1"));
        let ttl: i64 = cache.conn().ttl(&key).await.unwrap();
        assert!(ttl > 5, "reading extended it to {ttl}");
        let _: () = cache.conn().del(&key).await.unwrap();
    }

    #[tokio::test]
    async fn heartbeats_list_live_instances() {
        let Some(cache) = test_cache() else { return };
        let (live, lapsed) = (format!("test-{}", Uuid::new_v4()), format!("test-{}", Uuid::new_v4()));
        cache.heartbeat(&lapsed, &serde_json::json!({ "id": lapsed }).to_string(), 1).await;
        cache.heartbeat(&live, &serde_json::json!({ "id": live }).to_string(), 30).await;
        let ids = |v: Vec<serde_json::Value>| v.into_iter().filter_map(|i| i["id"].as_str().map(String::from)).collect::<Vec<_>>();
        let now = ids(cache.instances().await);
        assert!(now.contains(&live) && now.contains(&lapsed), "{now:?}");
        tokio::time::sleep(Duration::from_millis(2100)).await;
        let later = ids(cache.instances().await);
        assert!(later.contains(&live) && !later.contains(&lapsed), "{later:?}");
        let _: () = cache.conn().hdel(INSTANCES_KEY, &[&live, &lapsed]).await.unwrap();
        let _: () = cache.conn().zrem(INSTANCES_SEEN_KEY, &[&live, &lapsed]).await.unwrap();
    }

    #[tokio::test]
    async fn saved_conversations_are_indexed() {
        let Some(cache) = test_cache() else { return };
        let root = format!("test-{}", Uuid::new_v4());
        cache.save_turn(&root, "resp_1", b"sealed", Duration::from_secs(60)).await.unwrap();
        let key = format!("gw:conv:{{{root}}}");
        assert!(cache.conversation_keys().await.unwrap().contains(&key));
        cache.replace_turns(&key, &[("resp_1".into(), b"resealed".to_vec())]).await.unwrap();
        assert_eq!(cache.conversation_turns(&key).await.unwrap()["resp_1"], b"resealed");
        let _: () = cache.conn().del(&key).await.unwrap();
        let _: () = cache.conn().zrem(CONVERSATIONS_KEY, &root).await.unwrap();
    }

    #[tokio::test]
    async fn sync_messages_reach_subscribers() {
        let Some(cache) = test_cache() else { return };
        // Not the sync channel: a gateway running on the same Valkey would get the messages.
        const CHANNEL: &str = "gw:test";
        let mut messages = cache.subscribe(CHANNEL).await.unwrap();
        let payload = format!("test-{}", Uuid::new_v4());
        // Other test runs may publish too; wait for this one.
        let received = async {
            while let Some(msg) = messages.next().await {
                if msg.get_payload::<String>().unwrap() == payload {
                    return;
                }
            }
            panic!("subscription ended");
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(received, async {
                tokio::time::sleep(Duration::from_millis(100)).await;
                let _: i64 = cache.conn().publish(CHANNEL, &payload).await.unwrap();
            })
        })
        .await
        .unwrap();
    }
}
