//! `inference-gateway rotate-key`: re-encrypts everything sealed with the encryption key after
//! the key changed. It runs with the same environment as the replicas during a rotation: the new
//! key in `GATEWAY_ENCRYPTION_KEY` and the retired one in `GATEWAY_PREVIOUS_ENCRYPTION_KEY` (the
//! procedure is in docs/production.md). Values already under the new key are skipped, so it can run again
//! to catch anything saved in between; a stored value that neither key opens stops the run
//! before anything is written.

use crate::{cache::Cache, config::Config, db};

pub async fn run() -> Result<(), String> {
    let cfg = Config::from_env()?;
    if !cfg.secret_box.has_previous() {
        return Err("set GATEWAY_ENCRYPTION_KEY to the new key and GATEWAY_PREVIOUS_ENCRYPTION_KEY to the one being retired".into());
    }
    let sb = &cfg.secret_box;
    let pg = |e: sqlx::Error| format!("postgres: {e}");
    let pool = db::connect(&cfg.database_url, 2).await.map_err(pg)?;

    // Postgres: one transaction, so a value neither key opens leaves everything as it was.
    let mut tx = pool.begin().await.map_err(pg)?;
    let (mut moved, mut kept) = (0usize, 0usize);
    for (id, name, sealed) in db::sealed_credentials(&mut *tx).await.map_err(pg)? {
        match sb.reseal(&sealed).map_err(|e| format!("credential '{name}' ({id}): {e}"))? {
            Some(v) => {
                db::set_credential_secrets(&mut *tx, id, &v).await.map_err(pg)?;
                moved += 1;
            }
            None => kept += 1,
        }
    }
    println!("credentials: {moved} re-encrypted, {kept} already under the current key");
    let (mut moved, mut kept) = (0usize, 0usize);
    for (id, name, sealed) in db::sealed_mcp_secrets(&mut *tx).await.map_err(pg)? {
        match sb.reseal(&sealed).map_err(|e| format!("MCP server '{name}' ({id}): {e}"))? {
            Some(v) => {
                db::set_mcp_secret(&mut *tx, id, &v).await.map_err(pg)?;
                moved += 1;
            }
            None => kept += 1,
        }
    }
    println!("MCP servers: {moved} secrets re-encrypted, {kept} already under the current key");
    let (mut moved, mut kept) = (0usize, 0usize);
    for (id, name, sealed) in db::sealed_a2a_secrets(&mut *tx).await.map_err(pg)? {
        match sb.reseal(&sealed).map_err(|e| format!("A2A agent '{name}' ({id}): {e}"))? {
            Some(v) => {
                db::set_a2a_secret(&mut *tx, id, &v).await.map_err(pg)?;
                moved += 1;
            }
            None => kept += 1,
        }
    }
    println!("A2A agents: {moved} secrets re-encrypted, {kept} already under the current key");
    let (mut moved, mut kept) = (0usize, 0usize);
    for (kind, upstream, key, labels, access, refresh) in db::sealed_oauth_tokens(&mut *tx).await.map_err(pg)? {
        let what = || format!("OAuth tokens of key {key} for {kind} {upstream}");
        let access_new = sb.reseal(&access).map_err(|e| format!("{}: {e}", what()))?;
        let refresh_new = refresh.as_deref().map(|r| sb.reseal(r)).transpose().map_err(|e| format!("{}: {e}", what()))?.flatten();
        if access_new.is_none() && refresh_new.is_none() {
            kept += 1;
            continue;
        }
        let access = access_new.unwrap_or(access);
        let refresh = refresh_new.or(refresh);
        let s = db::SignInRef { kind: &kind, upstream, key, labels: &labels };
        db::set_oauth_token_secrets(&mut *tx, s, &access, refresh.as_deref()).await.map_err(pg)?;
        moved += 1;
    }
    println!("upstream sign-ins: {moved} re-encrypted, {kept} already under the current key");
    match db::sealed_oidc_client_secret(&mut *tx).await.map_err(pg)? {
        Some(sealed) => match sb.reseal(&sealed).map_err(|e| format!("OIDC client secret: {e}"))? {
            Some(v) => {
                db::set_oidc_client_secret(&mut *tx, &v).await.map_err(pg)?;
                println!("OIDC client secret: re-encrypted");
            }
            None => println!("OIDC client secret: already under the current key"),
        },
        None => println!("OIDC client secret: none stored"),
    }
    tx.commit().await.map_err(pg)?;

    // Valkey: conversation state for `previous_response_id`. It expires on its own, so a turn
    // neither key opens is counted and left alone rather than stopping the run.
    let vk = |e: redis::RedisError| format!("valkey: {e}");
    let cache = Cache::new(&cfg.valkey_url, cfg.valkey_cluster).map_err(|e| format!("invalid VALKEY_URL: {e}"))?;
    let keys = cache.conversation_keys().await.map_err(vk)?;
    let (mut turns, mut conversations, mut unreadable) = (0usize, 0usize, 0usize);
    for key in &keys {
        let mut updated = Vec::new();
        for (id, sealed) in cache.conversation_turns(key).await.map_err(vk)? {
            match sb.reseal(&sealed) {
                Ok(Some(v)) => updated.push((id, v)),
                Ok(None) => {}
                Err(_) => unreadable += 1,
            }
        }
        if !updated.is_empty() {
            turns += updated.len();
            conversations += 1;
            cache.replace_turns(key, &updated).await.map_err(vk)?;
        }
    }
    println!("conversations: {turns} turns re-encrypted in {conversations} of {} conversations", keys.len());
    if unreadable > 0 {
        println!("conversations: {unreadable} turns could not be read with either key and were left as they are");
    }
    println!("done; GATEWAY_PREVIOUS_ENCRYPTION_KEY can be removed from the environment");
    Ok(())
}
