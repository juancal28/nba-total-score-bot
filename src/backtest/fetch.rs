//! Downloads and caches everything the backtest replays: SportRadar
//! schedules and play-by-play, and Polymarket events and price histories.
//! Every file is fetched once; reruns only fill in what is missing.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use futures::StreamExt;
use rand::SeedableRng;
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::config::Config;
use crate::paper::{ScheduledGame, SportRadar, parse_daily_schedule};
use crate::polymarket::public::{
    event_slug, fetch_event_raw, fetch_price_history_raw, main_totals_market,
};

pub const ROOT: &str = "data/backtest";
/// SportRadar season years: 2025 = the 2025-26 season, 2026 = the current one.
pub const LAST_SEASON: u16 = 2025;
pub const CURRENT_SEASON: u16 = 2026;

pub struct FetchArgs {
    /// Games to sample from last season.
    pub last_season_games: usize,
    pub seed: u64,
    /// Re-download the current-season schedule (1 request) to pick up games
    /// played since the last fetch.
    pub refresh_current: bool,
    /// Stop making SportRadar requests after this many in this run.
    pub max_requests: u32,
}

/// One game in the backtest sample.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SampleGame {
    pub id: String,
    pub season: u16,
    pub scheduled: jiff::Timestamp,
    pub away: String,
    pub home: String,
    pub slug: String,
}

pub fn schedule_path(season: u16) -> PathBuf {
    Path::new(ROOT).join(format!("sportradar/schedule_{season}_REG.json"))
}
pub fn pbp_path(game_id: &str) -> PathBuf {
    Path::new(ROOT).join(format!("sportradar/pbp_{game_id}.json"))
}
pub fn event_path(slug: &str) -> PathBuf {
    Path::new(ROOT).join(format!("polymarket/events/{slug}.json"))
}
pub fn prices_path(token_id: &str) -> PathBuf {
    Path::new(ROOT).join(format!("polymarket/prices/{token_id}.json"))
}
pub fn sample_path() -> PathBuf {
    Path::new(ROOT).join("sample.json")
}

pub fn load_sample() -> Result<Vec<SampleGame>> {
    let body = std::fs::read(sample_path()).context("no sample yet; run `backtest fetch` first")?;
    Ok(serde_json::from_slice(&body)?)
}

fn write_file(path: &Path, body: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, body)?;
    Ok(())
}

/// Pick `n` games spread evenly across calendar months: shuffle each month
/// with the seeded RNG, then deal one game per month in rotation.
pub fn stratified_sample(mut games: Vec<SampleGame>, n: usize, seed: u64) -> Vec<SampleGame> {
    games.sort_by(|a, b| a.id.cmp(&b.id));
    let mut by_month: BTreeMap<String, Vec<SampleGame>> = BTreeMap::new();
    for g in games {
        by_month.entry(g.scheduled.to_string()[..7].to_string()).or_default().push(g);
    }
    let mut rng = StdRng::seed_from_u64(seed);
    for month in by_month.values_mut() {
        month.shuffle(&mut rng);
    }
    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        let before = out.len();
        for month in by_month.values_mut() {
            if out.len() < n {
                if let Some(g) = month.pop() {
                    out.push(g);
                }
            }
        }
        if out.len() == before {
            break; // every month exhausted
        }
    }
    out.sort_by(|a, b| a.scheduled.cmp(&b.scheduled).then_with(|| a.id.cmp(&b.id)));
    out
}

pub async fn run(args: FetchArgs) -> Result<()> {
    let cfg = Config::load()?;
    let api_key = cfg
        .sportradar
        .api_key
        .clone()
        .ok_or_else(|| anyhow!("NPB_SPORTRADAR__API_KEY is not set"))?;
    let http = reqwest::Client::builder().timeout(Duration::from_secs(60)).build()?;
    let sr = SportRadar::new(http.clone(), cfg.sportradar.base_url.clone(), api_key);

    // 1. Season schedules.
    let mut closed: Vec<SampleGame> = Vec::new();
    for season in [LAST_SEASON, CURRENT_SEASON] {
        let path = schedule_path(season);
        if !path.exists() || (season == CURRENT_SEASON && args.refresh_current) {
            let body = sr.get(&format!("/games/{season}/REG/schedule.json")).await?;
            write_file(&path, &body)?;
        }
        let games: Vec<ScheduledGame> = parse_daily_schedule(&std::fs::read(&path)?)?;
        let n_closed = games.iter().filter(|g| g.status == "closed").count();
        info!(season, games = games.len(), closed = n_closed, "schedule loaded");
        closed.extend(games.into_iter().filter(|g| g.status == "closed").map(|g| SampleGame {
            slug: event_slug(&g.away.alias, &g.home.alias, g.scheduled),
            id: g.id,
            season,
            scheduled: g.scheduled,
            away: g.away.alias,
            home: g.home.alias,
        }));
    }

    // 2. Polymarket event for every closed game (free, so check them all).
    let missing: Vec<&SampleGame> = closed.iter().filter(|g| !event_path(&g.slug).exists()).collect();
    info!(missing = missing.len(), "fetching polymarket events");
    let http_ref = &http;
    futures::stream::iter(missing)
        .for_each_concurrent(6, |g| async move {
            match fetch_event_raw(http_ref, &g.slug).await {
                Ok(body) => {
                    if let Err(e) = write_file(&event_path(&g.slug), &body) {
                        warn!(slug = %g.slug, error = ?e, "could not cache event");
                    }
                }
                Err(e) => warn!(slug = %g.slug, error = ?e, "event fetch failed"),
            }
        })
        .await;

    let has_market = |g: &SampleGame| -> bool {
        std::fs::read(event_path(&g.slug))
            .ok()
            .and_then(|b| main_totals_market(&b).ok().flatten())
            .is_some()
    };
    let with_market: Vec<SampleGame> = closed.iter().filter(|g| has_market(g)).cloned().collect();
    for season in [LAST_SEASON, CURRENT_SEASON] {
        info!(
            season,
            closed = closed.iter().filter(|g| g.season == season).count(),
            with_market = with_market.iter().filter(|g| g.season == season).count(),
            "polymarket coverage"
        );
    }

    // 3. Sample. Last season's draw is made once and then kept, so adding
    // current-season games later never changes which older games are used.
    let previous: Vec<SampleGame> = load_sample().unwrap_or_default();
    let mut sample: Vec<SampleGame> = if previous.iter().any(|g| g.season == LAST_SEASON) {
        previous.into_iter().filter(|g| g.season == LAST_SEASON).collect()
    } else {
        let last: Vec<SampleGame> =
            with_market.iter().filter(|g| g.season == LAST_SEASON).cloned().collect();
        stratified_sample(last, args.last_season_games, args.seed)
    };
    sample.extend(with_market.iter().filter(|g| g.season == CURRENT_SEASON).cloned());
    sample.sort_by(|a, b| a.scheduled.cmp(&b.scheduled).then_with(|| a.id.cmp(&b.id)));
    write_file(&sample_path(), &serde_json::to_vec_pretty(&sample)?)?;
    info!(games = sample.len(), "sample written");

    // 4. Play-by-play for the sample (the scarce resource).
    let todo: Vec<&SampleGame> = sample.iter().filter(|g| !pbp_path(&g.id).exists()).collect();
    info!(missing = todo.len(), budget = args.max_requests, "fetching play-by-play");
    for (i, g) in todo.iter().enumerate() {
        if sr.requests() >= args.max_requests {
            warn!(remaining = todo.len() - i, "request budget for this run reached; stopping");
            break;
        }
        let mut attempt = 0;
        loop {
            attempt += 1;
            match sr.get(&format!("/games/{}/pbp.json", g.id)).await {
                Ok(body) => {
                    write_file(&pbp_path(&g.id), &body)?;
                    break;
                }
                Err(e) if attempt < 3 => {
                    warn!(game = %g.id, error = %e, "pbp fetch failed; retrying");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
                Err(e) => {
                    warn!(game = %g.id, error = %e, "pbp fetch failed; skipping");
                    break;
                }
            }
        }
        if (i + 1) % 25 == 0 {
            info!(done = i + 1, of = todo.len(), requests = sr.requests(), "play-by-play progress");
        }
    }

    // 5. Price history for each sampled game's main line.
    let http = Arc::new(http);
    let jobs: Vec<(String, i64)> = sample
        .iter()
        .filter_map(|g| {
            let market = main_totals_market(&std::fs::read(event_path(&g.slug)).ok()?).ok()??;
            (!prices_path(&market.over_token_id).exists())
                .then(|| (market.over_token_id, g.scheduled.as_second()))
        })
        .collect();
    info!(missing = jobs.len(), "fetching price histories");
    futures::stream::iter(jobs)
        .for_each_concurrent(4, |(token, start)| {
            let http = http.clone();
            async move {
                match fetch_price_history_raw(&http, &token, start - 3600, start + 5 * 3600).await {
                    Ok(body) => {
                        if let Err(e) = write_file(&prices_path(&token), &body) {
                            warn!(%token, error = ?e, "could not cache prices");
                        }
                    }
                    Err(e) => warn!(%token, error = %e, "price history fetch failed"),
                }
            }
        })
        .await;

    info!(sportradar_requests = sr.requests(), "fetch complete");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn game(id: u32, month: u32) -> SampleGame {
        SampleGame {
            id: format!("g{id:03}"),
            season: 2025,
            scheduled: format!("2025-{month:02}-15T00:00:00Z").parse().unwrap(),
            away: "AAA".into(),
            home: "BBB".into(),
            slug: format!("nba-aaa-bbb-{id}"),
        }
    }

    fn pool() -> Vec<SampleGame> {
        // 30 games in October, 30 in November, 4 in December.
        (0..30).map(|i| game(i, 10)).chain((30..60).map(|i| game(i, 11))).chain((60..64).map(|i| game(i, 12))).collect()
    }

    fn month_count(sample: &[SampleGame], month: &str) -> usize {
        sample.iter().filter(|g| g.scheduled.to_string()[5..7] == *month).count()
    }

    #[test]
    fn sample_is_spread_across_months() {
        let s = stratified_sample(pool(), 24, 42);
        assert_eq!(s.len(), 24);
        // December only has 4; the rest split evenly between the other two.
        assert_eq!(month_count(&s, "12"), 4);
        assert_eq!(month_count(&s, "10"), 10);
        assert_eq!(month_count(&s, "11"), 10);
    }

    #[test]
    fn sample_is_reproducible_and_seed_dependent() {
        let mut shuffled = pool();
        shuffled.reverse();
        assert_eq!(stratified_sample(pool(), 24, 42), stratified_sample(shuffled, 24, 42));
        assert_ne!(stratified_sample(pool(), 24, 42), stratified_sample(pool(), 24, 43));
    }

    #[test]
    fn sample_never_exceeds_the_pool() {
        assert_eq!(stratified_sample(pool(), 500, 42).len(), 64);
    }
}
