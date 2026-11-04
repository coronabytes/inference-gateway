//! Budget alerts: projects and keys email the people responsible for them when their spend in the
//! current budget period crosses a threshold, such as 80 % and 99 % of the budget.
//!
//! Every replica checks once a minute. An alert is claimed in Postgres (`budget_alerts_sent`)
//! before it is sent, so it goes out once per budget period whichever replica sees it first. When
//! several thresholds are crossed at once, one email names the highest. A failed send gives the
//! claim back, so the next check tries again.

use std::{collections::HashMap, time::Duration};

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::{
    budget::{BudgetPeriod, BudgetStatus},
    db::{self, AlertOwner},
    mail::{Mail, Mailer, escape},
    state::AppState,
};

/// Thresholds are whole percents of the budget.
pub const THRESHOLDS: std::ops::RangeInclusive<i16> = 1..=100;
/// Most thresholds and recipients one project or key may have.
pub const MAX_THRESHOLDS: usize = 10;
pub const MAX_RECIPIENTS: usize = 50;

const CHECK_EVERY: Duration = Duration::from_secs(60);

pub async fn run(state: AppState) {
    let mut tick = tokio::time::interval(CHECK_EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut checks = 0u64;
    loop {
        tick.tick().await;
        if let Err(e) = check(&state).await {
            tracing::warn!(error = %e, "checking budget alerts failed");
        }
        checks += 1;
        if checks.is_multiple_of(60) {
            match db::prune_budget_alerts_sent(&state.db).await {
                Ok(n) if n > 0 => tracing::debug!(deleted = n, "pruned old budget alerts"),
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "pruning old budget alerts failed"),
            }
        }
    }
}

/// A project's or key's spend, for one alert.
struct Watched {
    spender: String,
    owner: Owner,
    status: BudgetStatus,
}

/// What an alert is about, for its email.
#[derive(Debug, Clone)]
pub enum Owner {
    Project { name: String },
    Key { name: String, prefix: String, project: Option<String> },
}

impl Owner {
    fn noun(&self) -> &'static str {
        match self {
            Owner::Project { .. } => "project",
            Owner::Key { .. } => "key",
        }
    }

    fn name(&self) -> &str {
        match self {
            Owner::Project { name } | Owner::Key { name, .. } => name,
        }
    }
}

/// Checks every project and key with an alert, and emails what crossed a threshold.
async fn check(state: &AppState) -> Result<(), String> {
    let rules = db::list_budget_alerts(&state.db).await.map_err(|e| e.to_string())?;
    if rules.is_empty() {
        return Ok(());
    }
    // Nothing is claimed while mail is off: alerts go out once it is set up, if still due.
    let Some(mailer) = Mailer::load(state).await? else { return Ok(()) };
    let snap = state.registry.snapshot();
    let keys: HashMap<Uuid, _> = snap.keys.values().map(|k| (k.id, k.clone())).collect();
    let now = Utc::now();
    for rule in rules {
        let watched = match (rule.project_id, rule.key_id) {
            (Some(id), _) => match snap.projects.get(&id) {
                Some(p) => crate::budget::project_status(state, p).await.map(|status| Watched {
                    spender: spender(AlertOwner::Project(id)),
                    owner: Owner::Project { name: p.name.clone() },
                    status,
                }),
                None => None,
            },
            (None, Some(id)) => match keys.get(&id) {
                Some(k) => crate::budget::status(state, k).await.map(|status| Watched {
                    spender: spender(AlertOwner::Key(id)),
                    owner: Owner::Key {
                        name: k.name.clone(),
                        prefix: k.key_prefix.clone(),
                        project: k.project_id.and_then(|p| snap.projects.get(&p)).map(|p| p.name.clone()),
                    },
                    status,
                }),
                None => None,
            },
            (None, None) => None,
        };
        let Some(w) = watched else { continue };
        if w.status.unknown || w.status.limit_usd <= 0.0 {
            continue;
        }
        let percent = w.status.spent_usd / w.status.limit_usd * 100.0;
        let window = w.status.period.window(now).id;
        let mut claimed = Vec::new();
        for &t in rule.thresholds.iter().filter(|&&t| percent >= f64::from(t)) {
            if db::claim_budget_alert(&state.db, &w.spender, &window, t).await.map_err(|e| e.to_string())? {
                claimed.push(t);
            }
        }
        let Some(&threshold) = claimed.iter().max() else { continue };
        let people = db::people_emails(&state.db, &rule.recipients).await.map_err(|e| e.to_string())?;
        if people.is_empty() {
            tracing::warn!(spender = %w.spender, threshold, "budget alert has nobody with an email address to send to");
            continue;
        }
        let to: Vec<(String, Option<String>)> = people.into_iter().map(|(_, email, name)| (email, name)).collect();
        let mail = budget_email(&BudgetEmail { owner: &w.owner, threshold, status: &w.status, console_url: mailer.console_url.as_deref() });
        match mailer.send(&to, &mail).await {
            Ok(()) => tracing::info!(spender = %w.spender, threshold, recipients = to.len(), "sent budget alert"),
            Err(e) => {
                tracing::warn!(spender = %w.spender, threshold, error = %e, "could not send budget alert; retrying on the next check");
                db::release_budget_alerts(&state.db, &w.spender, &window, &claimed).await.map_err(|e| e.to_string())?;
            }
        }
    }
    Ok(())
}

/// The spender's name in `budget_alerts_sent`.
pub fn spender(owner: AlertOwner) -> String {
    match owner {
        AlertOwner::Project(id) => format!("project:{id}"),
        AlertOwner::Key(id) => format!("key:{id}"),
    }
}

/// After a spend reset: the current periods' alerts may go out again.
pub async fn forget_current(state: &AppState, owner: AlertOwner) {
    let now = Utc::now();
    let windows: Vec<String> = BudgetPeriod::ALL.iter().map(|p| p.window(now).id).collect();
    if let Err(e) = db::forget_budget_alerts(&state.db, &spender(owner), &windows).await {
        tracing::warn!(error = %e, "could not clear sent budget alerts after a spend reset");
    }
}

/// What a budget alert email says.
pub struct BudgetEmail<'a> {
    pub owner: &'a Owner,
    pub threshold: i16,
    pub status: &'a BudgetStatus,
    pub console_url: Option<&'a str>,
}

fn usd(v: f64) -> String {
    let cents = (v * 100.0).round() as i64;
    let (whole, frac) = (cents.abs() / 100, cents.abs() % 100);
    let digits = whole.to_string();
    let mut grouped = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(c);
    }
    format!("{}${grouped}.{frac:02}", if cents < 0 { "-" } else { "" })
}

/// The budget period in words: "today", "this week", "this month".
fn period_words(p: BudgetPeriod) -> (&'static str, &'static str) {
    match p {
        BudgetPeriod::Day => ("daily", "today"),
        BudgetPeriod::Week => ("weekly", "this week"),
        BudgetPeriod::Month => ("monthly", "this month"),
    }
}

/// The alert email: a headline, a meter of the budget used, the figures, and a link to the console.
pub fn budget_email(e: &BudgetEmail) -> Mail {
    let s = e.status;
    let percent = if s.limit_usd > 0.0 { s.spent_usd / s.limit_usd * 100.0 } else { 0.0 };
    let shown = percent.floor() as i64;
    let (adjective, when) = period_words(s.period);
    let noun = e.owner.noun();
    let name = e.owner.name();
    let exhausted = s.spent_usd >= s.limit_usd;
    let subject = if exhausted {
        format!("Budget used up: {noun} {name} has spent its {adjective} budget")
    } else {
        format!("Budget alert: {noun} {name} has used {shown}% of its {adjective} budget")
    };
    let headline = if exhausted {
        format!("The {noun} <strong>{}</strong> has used up its {adjective} budget", escape(name))
    } else {
        format!("The {noun} <strong>{}</strong> has used {shown}% of its {adjective} budget", escape(name))
    };
    let consequence = if exhausted {
        match e.owner {
            Owner::Project { .. } => "Requests of the project's keys are refused until the budget resets or is raised.",
            Owner::Key { .. } => "Requests with this key are refused until the budget resets or is raised.",
        }
    } else {
        "Requests are refused once the whole budget is spent, until it resets or is raised."
    };
    // Amber up to 90 %, red from there.
    let accent = if percent >= 90.0 { "#dc2626" } else { "#d97706" };
    let fill = percent.clamp(0.0, 100.0).round() as i64;
    let date = |d: DateTime<Utc>| d.format("%b %-d, %Y, %H:%M UTC").to_string();
    let remaining = (s.limit_usd - s.spent_usd).max(0.0);
    let mut rows = vec![
        ("Spent", format!("{} of {}", usd(s.spent_usd), usd(s.limit_usd))),
        ("Remaining", usd(remaining)),
        ("Alert threshold", format!("{}%", e.threshold)),
        ("Resets", date(s.resets_at)),
    ];
    if let Owner::Key { prefix, project, .. } = e.owner {
        rows.insert(0, ("Key", prefix.clone()));
        if let Some(p) = project {
            rows.insert(1, ("Project", p.clone()));
        }
    }
    let table: String = rows
        .iter()
        .map(|(k, v)| {
            format!(
                r#"<tr><td style="padding:8px 0;color:#6b7280;font-size:14px;border-top:1px solid #e5e7eb;">{k}</td><td align="right" style="padding:8px 0;color:#111827;font-size:14px;font-weight:600;border-top:1px solid #e5e7eb;">{}</td></tr>"#,
                escape(v)
            )
        })
        .collect();
    let link = e.console_url.map(|u| format!("{}/#/access", u.trim_end_matches('/')));
    let button = link.as_deref().map_or(String::new(), |href| {
        format!(
            r#"<tr><td style="padding:8px 32px 32px;"><a href="{}" style="display:inline-block;background:#111827;color:#ffffff;text-decoration:none;font-size:14px;font-weight:600;padding:12px 20px;border-radius:8px;">Open the console</a></td></tr>"#,
            escape(href)
        )
    });
    let bar = if fill >= 100 {
        format!(r#"<td style="background:{accent};height:10px;border-radius:5px;font-size:0;line-height:0;">&nbsp;</td>"#)
    } else if fill <= 0 {
        r#"<td style="background:#e5e7eb;height:10px;border-radius:5px;font-size:0;line-height:0;">&nbsp;</td>"#.to_string()
    } else {
        format!(
            r#"<td width="{fill}%" style="background:{accent};height:10px;border-radius:5px 0 0 5px;font-size:0;line-height:0;">&nbsp;</td><td width="{}%" style="background:#e5e7eb;height:10px;border-radius:0 5px 5px 0;font-size:0;line-height:0;">&nbsp;</td>"#,
            100 - fill
        )
    };
    let html = format!(
        r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><meta name="color-scheme" content="light"><title>{title}</title></head>
<body style="margin:0;padding:0;background:#f3f4f6;font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Helvetica,Arial,sans-serif;">
<div style="display:none;max-height:0;overflow:hidden;">{preheader}</div>
<table role="presentation" width="100%" cellpadding="0" cellspacing="0" style="background:#f3f4f6;padding:32px 16px;"><tr><td align="center">
<table role="presentation" width="100%" cellpadding="0" cellspacing="0" style="max-width:560px;background:#ffffff;border-radius:12px;overflow:hidden;border:1px solid #e5e7eb;">
<tr><td style="background:{accent};height:4px;font-size:0;line-height:0;">&nbsp;</td></tr>
<tr><td style="padding:28px 32px 0;color:#6b7280;font-size:12px;font-weight:600;letter-spacing:.08em;text-transform:uppercase;">Inference Gateway · Budget alert</td></tr>
<tr><td style="padding:12px 32px 0;color:#111827;font-size:22px;line-height:1.35;font-weight:500;">{headline}</td></tr>
<tr><td style="padding:24px 32px 0;">
<table role="presentation" width="100%" cellpadding="0" cellspacing="0"><tr>
<td style="color:{accent};font-size:40px;font-weight:700;line-height:1;">{shown}%</td>
<td align="right" style="color:#6b7280;font-size:14px;vertical-align:bottom;">{spent} of {limit} {when}</td>
</tr></table>
<table role="presentation" width="100%" cellpadding="0" cellspacing="0" style="margin-top:12px;"><tr>{bar}</tr></table>
</td></tr>
<tr><td style="padding:24px 32px 8px;"><table role="presentation" width="100%" cellpadding="0" cellspacing="0">{table}</table></td></tr>
<tr><td style="padding:8px 32px 24px;color:#374151;font-size:14px;line-height:1.5;">{consequence}</td></tr>
{button}
</table>
<p style="max-width:560px;margin:16px auto 0;color:#9ca3af;font-size:12px;line-height:1.5;text-align:center;">You get this email because you are listed as responsible for this {noun}'s budget alerts. Each threshold is sent once per budget period.</p>
</td></tr></table>
</body></html>"#,
        title = escape(&subject),
        preheader = escape(&format!("{} of {} spent {when}.", usd(s.spent_usd), usd(s.limit_usd))),
        spent = usd(s.spent_usd),
        limit = usd(s.limit_usd),
    );
    let mut text = format!("{}\n\n", subject);
    for (k, v) in &rows {
        text.push_str(&format!("{k}: {v}\n"));
    }
    text.push_str(&format!("\n{consequence}\n"));
    if let Some(href) = &link {
        text.push_str(&format!("\nOpen the console: {href}\n"));
    }
    Mail { subject, html, text }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(spent: f64, limit: f64) -> BudgetStatus {
        BudgetStatus {
            period: BudgetPeriod::Month,
            limit_usd: limit,
            spent_usd: spent,
            resets_at: DateTime::parse_from_rfc3339("2026-12-01T00:00:00Z").unwrap().with_timezone(&Utc),
            unknown: false,
        }
    }

    #[test]
    fn dollars() {
        assert_eq!(usd(0.0), "$0.00");
        assert_eq!(usd(82.105), "$82.11");
        assert_eq!(usd(1234567.5), "$1,234,567.50");
    }

    #[test]
    fn email_names_the_project_and_the_share_used() {
        let owner = Owner::Project { name: "search <team>".into() };
        let s = status(82.5, 100.0);
        let m = budget_email(&BudgetEmail { owner: &owner, threshold: 80, status: &s, console_url: Some("https://gw.example.com/") });
        assert_eq!(m.subject, "Budget alert: project search <team> has used 82% of its monthly budget");
        assert!(m.html.contains("search &lt;team&gt;"), "names are escaped");
        assert!(m.html.contains("https://gw.example.com/#/access"));
        assert!(m.text.contains("Spent: $82.50 of $100.00"));
        assert!(m.text.contains("Alert threshold: 80%"));

        let key = Owner::Key { name: "ci".into(), prefix: "sk-gw-ab12".into(), project: Some("search".into()) };
        let spent = status(100.0, 100.0);
        let m = budget_email(&BudgetEmail { owner: &key, threshold: 99, status: &spent, console_url: None });
        assert_eq!(m.subject, "Budget used up: key ci has spent its monthly budget");
        assert!(m.text.contains("Project: search"));
        assert!(!m.html.contains("Open the console"));
    }
}
