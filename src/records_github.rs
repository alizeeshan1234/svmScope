//! The durable 30-day queue behind a record store, on GitHub release assets.
//!
//! Git history never shrinks, so a rolling window cannot live in commits.
//! Release *assets* are stored outside the history, can be deleted freely,
//! and cost nothing: one release per UTC day, one asset per hour holding
//! that hour's pack of new versions ([`super::LogStore::export_since`]).
//! Push: at the end of each hour, upload the pack to today's release. Pop:
//! delete the release from 31 days ago. Restore after a redeploy: download
//! the last 30 days of assets and import them.
//!
//! Needs a repository and a token with contents and releases write scope on
//! it, and nothing else. Every call is a plain REST request through the
//! blocking client the RPC library already links.

use {
    super::LogStore,
    crate::error::{Error, Result},
    std::time::{Duration, SystemTime, UNIX_EPOCH},
};

/// A GitHub repository used as the queue.
pub struct GithubQueue {
    owner: String,
    repo: String,
    token: String,
    client: reqwest::blocking::Client,
}

/// How many daily releases to keep.
pub const KEEP_DAYS: u64 = 30;

/// A records release: its tag, id, and each asset's (name, API url).
pub type RecordRelease = (String, u64, Vec<(String, String)>);

fn api_err(context: &str, e: impl std::fmt::Display) -> Error {
    Error::Fixture(format!("records queue: {context}: {e}"))
}

/// `YYYY-MM-DD` of a Unix timestamp, UTC. Civil-from-days per Howard
/// Hinnant, no calendar dependency.
pub fn utc_date(unix_secs: u64) -> String {
    let days = (unix_secs / 86_400) as i64;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// The release tag for a UTC day.
pub fn tag_for(date: &str) -> String {
    format!("records-{date}")
}

/// The asset name for the UTC hour a Unix timestamp falls in.
pub fn asset_for(unix_secs: u64) -> String {
    format!("{:02}.pack", (unix_secs % 86_400) / 3_600)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl GithubQueue {
    /// A queue on `owner/repo`, authenticated with `token`.
    pub fn new(
        owner: impl Into<String>,
        repo: impl Into<String>,
        token: impl Into<String>,
    ) -> Result<GithubQueue> {
        let client = reqwest::blocking::Client::builder()
            .user_agent("svmscope-records")
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|e| api_err("client", e))?;
        Ok(GithubQueue {
            owner: owner.into(),
            repo: repo.into(),
            token: token.into(),
            client,
        })
    }

    fn api(&self, path: &str) -> String {
        format!(
            "https://api.github.com/repos/{}/{}{path}",
            self.owner, self.repo
        )
    }

    fn get(&self, url: &str) -> Result<serde_json::Value> {
        let resp = self
            .client
            .get(url)
            .bearer_auth(&self.token)
            .header("Accept", "application/vnd.github+json")
            .send()
            .map_err(|e| api_err("get", e))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(serde_json::Value::Null);
        }
        let resp = resp.error_for_status().map_err(|e| api_err("get", e))?;
        resp.json().map_err(|e| api_err("get json", e))
    }

    /// The release for `date`, created if missing. Returns its id.
    pub fn ensure_release(&self, date: &str) -> Result<u64> {
        let tag = tag_for(date);
        let existing = self.get(&self.api(&format!("/releases/tags/{tag}")))?;
        if let Some(id) = existing["id"].as_u64() {
            return Ok(id);
        }
        let body = serde_json::json!({
            "tag_name": tag,
            "name": format!("records {date}"),
            "body": "svmscope recorded account versions for this UTC day; one asset per hour. Deleted after 30 days.",
            "draft": false,
            "prerelease": true,
        });
        let created = match self.create_release(&body)? {
            Some(v) => v,
            None => {
                // GitHub refuses releases on a repository with no commits.
                // Give it one, saying what the repository is for, and retry.
                self.seed_readme()?;
                self.create_release(&body)?
                    .ok_or_else(|| api_err("create release", "still refused after seeding"))?
            }
        };
        created["id"]
            .as_u64()
            .ok_or_else(|| api_err("create release", "no id in response"))
    }

    /// `POST /releases`; `None` when GitHub answers 422 (an empty repository).
    fn create_release(&self, body: &serde_json::Value) -> Result<Option<serde_json::Value>> {
        let resp = self
            .client
            .post(self.api("/releases"))
            .bearer_auth(&self.token)
            .header("Accept", "application/vnd.github+json")
            .json(body)
            .send()
            .map_err(|e| api_err("create release", e))?;
        if resp.status() == reqwest::StatusCode::UNPROCESSABLE_ENTITY {
            return Ok(None);
        }
        let resp = resp
            .error_for_status()
            .map_err(|e| api_err("create release", e))?;
        resp.json()
            .map(Some)
            .map_err(|e| api_err("create release json", e))
    }

    /// The one commit a records repository needs: a README saying what it is.
    fn seed_readme(&self) -> Result<()> {
        use base64::Engine;
        let text = format!(
            "# {}\n\nRecorded Solana account versions for svmscope's free exact-replay tier: \
             one release per UTC day, one asset per hour, deleted after {KEEP_DAYS} days. \
             Nothing lives in the git history.\n",
            self.repo
        );
        let body = serde_json::json!({
            "message": "what this repository holds",
            "content": base64::engine::general_purpose::STANDARD.encode(text),
        });
        self.client
            .put(self.api("/contents/README.md"))
            .bearer_auth(&self.token)
            .header("Accept", "application/vnd.github+json")
            .json(&body)
            .send()
            .map_err(|e| api_err("seed readme", e))?
            .error_for_status()
            .map_err(|e| api_err("seed readme", e))?;
        Ok(())
    }

    /// Upload `pack` as `name` on the release `release_id`, replacing an asset
    /// of the same name if one exists.
    pub fn upload(&self, release_id: u64, name: &str, pack: Vec<u8>) -> Result<()> {
        // Replace: delete an existing asset with this name first.
        let assets = self.get(&self.api(&format!("/releases/{release_id}/assets")))?;
        if let Some(list) = assets.as_array() {
            for a in list {
                if a["name"].as_str() == Some(name) {
                    if let Some(id) = a["id"].as_u64() {
                        let _ = self
                            .client
                            .delete(self.api(&format!("/releases/assets/{id}")))
                            .bearer_auth(&self.token)
                            .send();
                    }
                }
            }
        }
        let url = format!(
            "https://uploads.github.com/repos/{}/{}/releases/{release_id}/assets?name={name}",
            self.owner, self.repo
        );
        self.client
            .post(url)
            .bearer_auth(&self.token)
            .header("Content-Type", "application/octet-stream")
            .body(pack)
            .send()
            .map_err(|e| api_err("upload", e))?
            .error_for_status()
            .map_err(|e| api_err("upload", e))?;
        Ok(())
    }

    /// Push everything the store recorded after `since` as this hour's pack.
    /// Returns the pack size in bytes.
    pub fn push_hour(&self, store: &LogStore, since: u64) -> Result<usize> {
        let pack = store.export_since(since)?;
        if pack.is_empty() {
            return Ok(0);
        }
        let now = now_secs();
        let id = self.ensure_release(&utc_date(now))?;
        let len = pack.len();
        self.upload(id, &asset_for(now), pack)?;
        Ok(len)
    }

    /// The tag of the oldest day still kept: releases tagged below it are
    /// expired.
    pub fn cutoff_tag() -> String {
        tag_for(&utc_date(now_secs().saturating_sub(KEEP_DAYS * 86_400)))
    }

    /// Every release tagged as a records day, oldest first, as
    /// `(tag, release id, [(asset name, asset API url)])`.
    pub fn releases(&self) -> Result<Vec<RecordRelease>> {
        self.record_releases()
    }

    fn record_releases(&self) -> Result<Vec<RecordRelease>> {
        let list = self.get(&self.api("/releases?per_page=100"))?;
        let mut out = Vec::new();
        if let Some(arr) = list.as_array() {
            for r in arr {
                let Some(tag) = r["tag_name"].as_str() else {
                    continue;
                };
                if !tag.starts_with("records-") {
                    continue;
                }
                let Some(id) = r["id"].as_u64() else {
                    continue;
                };
                let assets = r["assets"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| {
                                Some((
                                    x["name"].as_str()?.to_string(),
                                    x["url"].as_str()?.to_string(),
                                ))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                out.push((tag.to_string(), id, assets));
            }
        }
        out.sort();
        Ok(out)
    }

    /// Download every asset of the last [`KEEP_DAYS`] releases into `store`.
    /// Returns how many versions were added.
    pub fn restore(&self, store: &LogStore) -> Result<usize> {
        let cutoff = Self::cutoff_tag();
        let mut added = 0;
        for (tag, _id, assets) in self.record_releases()? {
            if tag < cutoff {
                continue;
            }
            for (_name, url) in assets {
                let bytes = self
                    .client
                    .get(&url)
                    .bearer_auth(&self.token)
                    .header("Accept", "application/octet-stream")
                    .send()
                    .map_err(|e| api_err("download", e))?
                    .error_for_status()
                    .map_err(|e| api_err("download", e))?
                    .bytes()
                    .map_err(|e| api_err("download bytes", e))?;
                added += store.import_pack(&bytes)?;
            }
        }
        Ok(added)
    }

    /// Delete every records release older than [`KEEP_DAYS`], tag included.
    /// Returns how many were removed.
    pub fn pop_old(&self) -> Result<usize> {
        let cutoff = Self::cutoff_tag();
        let mut removed = 0;
        for (tag, id, _assets) in self.record_releases()? {
            if tag >= cutoff {
                continue;
            }
            self.client
                .delete(self.api(&format!("/releases/{id}")))
                .bearer_auth(&self.token)
                .send()
                .map_err(|e| api_err("delete release", e))?
                .error_for_status()
                .map_err(|e| api_err("delete release", e))?;
            let _ = self
                .client
                .delete(self.api(&format!("/git/refs/tags/{tag}")))
                .bearer_auth(&self.token)
                .send();
            removed += 1;
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_dates_and_hours_are_right() {
        assert_eq!(utc_date(0), "1970-01-01");
        assert_eq!(utc_date(951_782_400), "2000-02-29"); // leap day
        assert_eq!(utc_date(1_788_691_319), "2026-09-06"); // 10:41:59 UTC
        assert_eq!(asset_for(1_788_691_319), "10.pack");
        assert_eq!(tag_for("2026-09-05"), "records-2026-09-05");
    }

    #[test]
    fn tags_sort_by_date_so_a_cutoff_compares_as_a_string() {
        assert!(tag_for("2026-08-13") < tag_for("2026-09-12"));
        assert!(tag_for("2026-09-12") < tag_for("2026-10-01"));
    }
}
