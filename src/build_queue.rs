use crate::db::{delete_crate, Pool};
use crate::docbuilder::PackageKind;
use crate::error::Result;
use crate::storage::Storage;
use crate::utils::{get_config, get_crate_priority, report_error, set_config, ConfigName};
use crate::{Config, Index, Metrics, RustwideBuilder};
use anyhow::Context;

use crates_index_diff::Change;
use log::{debug, info};

use git2::Oid;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

#[derive(Debug, Clone, Eq, PartialEq, serde::Serialize)]
pub(crate) struct QueuedCrate {
    #[serde(skip)]
    id: i32,
    pub(crate) name: String,
    pub(crate) version: String,
    pub(crate) priority: i32,
    pub(crate) registry: Option<String>,
}

#[derive(Debug)]
pub struct BuildQueue {
    config: Arc<Config>,
    storage: Arc<Storage>,
    pub(crate) db: Pool,
    metrics: Arc<Metrics>,
    max_attempts: i32,
}

impl BuildQueue {
    pub fn new(
        db: Pool,
        metrics: Arc<Metrics>,
        config: Arc<Config>,
        storage: Arc<Storage>,
    ) -> Self {
        BuildQueue {
            max_attempts: config.build_attempts.into(),
            config,
            db,
            metrics,
            storage,
        }
    }

    pub fn last_seen_reference(&self) -> Result<Option<Oid>> {
        let mut conn = self.db.get()?;
        if let Some(value) = get_config::<String>(&mut conn, ConfigName::LastSeenIndexReference)? {
            return Ok(Some(Oid::from_str(&value)?));
        }
        Ok(None)
    }

    fn set_last_seen_reference(&self, oid: Oid) -> Result<()> {
        let mut conn = self.db.get()?;
        set_config(
            &mut conn,
            ConfigName::LastSeenIndexReference,
            oid.to_string(),
        )?;
        Ok(())
    }

    pub fn add_crate(
        &self,
        name: &str,
        version: &str,
        priority: i32,
        registry: Option<&str>,
    ) -> Result<()> {
        self.db.get()?.execute(
            "INSERT INTO queue (name, version, priority, registry) 
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (name, version) DO UPDATE
                SET priority = EXCLUDED.priority,
                    registry = EXCLUDED.registry,
                    attempt = 0
            ;",
            &[&name, &version, &priority, &registry],
        )?;
        Ok(())
    }

    pub(crate) fn pending_count(&self) -> Result<usize> {
        let res = self.db.get()?.query(
            "SELECT COUNT(*) FROM queue WHERE attempt < $1;",
            &[&self.max_attempts],
        )?;
        Ok(res[0].get::<_, i64>(0) as usize)
    }

    pub(crate) fn prioritized_count(&self) -> Result<usize> {
        let res = self.db.get()?.query(
            "SELECT COUNT(*) FROM queue WHERE attempt < $1 AND priority <= 0;",
            &[&self.max_attempts],
        )?;
        Ok(res[0].get::<_, i64>(0) as usize)
    }

    pub(crate) fn failed_count(&self) -> Result<usize> {
        let res = self.db.get()?.query(
            "SELECT COUNT(*) FROM queue WHERE attempt >= $1;",
            &[&self.max_attempts],
        )?;
        Ok(res[0].get::<_, i64>(0) as usize)
    }

    pub(crate) fn queued_crates(&self) -> Result<Vec<QueuedCrate>> {
        let query = self.db.get()?.query(
            "SELECT id, name, version, priority, registry
             FROM queue
             WHERE attempt < $1
             ORDER BY priority ASC, attempt ASC, id ASC",
            &[&self.max_attempts],
        )?;

        Ok(query
            .into_iter()
            .map(|row| QueuedCrate {
                id: row.get("id"),
                name: row.get("name"),
                version: row.get("version"),
                priority: row.get("priority"),
                registry: row.get("registry"),
            })
            .collect())
    }

    pub(crate) fn process_next_crate(
        &self,
        f: impl FnOnce(&QueuedCrate) -> Result<()>,
    ) -> Result<()> {
        let mut conn = self.db.get()?;
        let mut transaction = conn.transaction()?;

        // fetch the next available crate from the queue table.
        // We are using `SELECT FOR UPDATE` inside a transaction so
        // the QueuedCrate is locked until we are finished with it.
        // `SKIP LOCKED` here will enable another build-server to just
        // skip over taken (=locked) rows and start building the first
        // available one.
        let to_process = match transaction
            .query_opt(
                "SELECT id, name, version, priority, registry
                 FROM queue
                 WHERE attempt < $1
                 ORDER BY priority ASC, attempt ASC, id ASC
                 LIMIT 1 
                 FOR UPDATE SKIP LOCKED",
                &[&self.max_attempts],
            )?
            .map(|row| QueuedCrate {
                id: row.get("id"),
                name: row.get("name"),
                version: row.get("version"),
                priority: row.get("priority"),
                registry: row.get("registry"),
            }) {
            Some(krate) => krate,
            None => return Ok(()),
        };

        let res = f(&to_process).with_context(|| {
            format!(
                "Failed to build package {}-{} from queue",
                to_process.name, to_process.version
            )
        });
        self.metrics.total_builds.inc();
        match res {
            Ok(()) => {
                transaction.execute("DELETE FROM queue WHERE id = $1;", &[&to_process.id])?;
            }
            Err(e) => {
                // Increase attempt count
                let attempt: i32 = transaction
                    .query_one(
                        "UPDATE queue SET attempt = attempt + 1 WHERE id = $1 RETURNING attempt;",
                        &[&to_process.id],
                    )?
                    .get(0);

                if attempt >= self.max_attempts {
                    self.metrics.failed_builds.inc();
                }

                report_error(&e);
            }
        }

        transaction.commit()?;

        Ok(())
    }
}

/// Locking functions.
impl BuildQueue {
    /// Checks for the lock and returns whether it currently exists.
    pub fn is_locked(&self) -> Result<bool> {
        let mut conn = self.db.get()?;

        Ok(get_config::<bool>(&mut conn, ConfigName::QueueLocked)?.unwrap_or(false))
    }

    /// lock the queue. Daemon will check this lock and stop operating if it exists.
    pub fn lock(&self) -> Result<()> {
        let mut conn = self.db.get()?;
        set_config(&mut conn, ConfigName::QueueLocked, true)
    }

    /// unlock the queue.
    pub fn unlock(&self) -> Result<()> {
        let mut conn = self.db.get()?;
        set_config(&mut conn, ConfigName::QueueLocked, false)
    }
}

fn retry<T>(mut f: impl FnMut() -> Result<T>, max_attempts: u32) -> Result<T> {
    for attempt in 1.. {
        match f() {
            Ok(result) => return Ok(result),
            Err(err) => {
                if attempt > max_attempts {
                    return Err(err);
                } else {
                    let sleep_for = 2u32.pow(attempt);
                    log::warn!(
                        "got error on attempt {}, will try again after {}s:\n{:?}",
                        attempt,
                        sleep_for,
                        err
                    );
                    thread::sleep(Duration::from_secs(sleep_for as u64));
                }
            }
        }
    }
    unreachable!()
}

/// Index methods.
impl BuildQueue {
    /// Updates registry index repository and adds new crates into build queue.
    ///
    /// Returns the number of crates added
    pub fn get_new_crates(&self, index: &Index) -> Result<usize> {
        let mut conn = self.db.get()?;
        let diff = index.diff()?;
        let (mut changes, oid) = diff.peek_changes()?;
        let mut crates_added = 0;

        // I believe this will fix ordering of queue if we get more than one crate from changes
        changes.reverse();

        for change in &changes {
            match change {
                Change::Yanked(release) => {
                    let res = conn
                        .execute(
                            "
                            UPDATE releases
                                SET yanked = TRUE
                            FROM crates
                            WHERE crates.id = releases.crate_id
                                AND name = $1
                                AND version = $2
                            ",
                            &[&release.name, &release.version],
                        )
                        .with_context(|| {
                            format!(
                                "error while setting {}-{} to yanked",
                                release.name, release.version
                            )
                        });
                    match res {
                        Ok(_) => debug!("{}-{} yanked", release.name, release.version),
                        Err(err) => report_error(&err),
                    }
                }

                Change::Added(release) => {
                    let priority = get_crate_priority(&mut conn, &release.name)?;

                    match self
                        .add_crate(
                            &release.name,
                            &release.version,
                            priority,
                            index.repository_url(),
                        )
                        .with_context(|| {
                            format!(
                                "failed adding {}-{} into build queue",
                                release.name, release.version
                            )
                        }) {
                        Ok(()) => {
                            debug!(
                                "{}-{} added into build queue",
                                release.name, release.version
                            );
                            crates_added += 1;
                        }
                        Err(err) => report_error(&err),
                    }
                }

                Change::Deleted(krate) => {
                    match delete_crate(&mut conn, &self.storage, &self.config, krate)
                        .with_context(|| format!("failed to delete crate {}", krate)) {
                            Ok(_) => info!("crate {} was deleted from the index and will be deleted from the database", krate), 
                            Err(err) => report_error(&err),
                        }
                }
            }
        }

        // additionally set the reference in the database
        // so this survives recreating the registry watcher
        // server.
        self.set_last_seen_reference(oid)?;

        // store the last seen reference as git reference in
        // the local crates.io index repo.
        diff.set_last_seen_reference(oid)?;

        Ok(crates_added)
    }

    /// Builds the top package from the queue. Returns whether there was a package in the queue.
    ///
    /// Note that this will return `Ok(true)` even if the package failed to build.
    pub(crate) fn build_next_queue_package(&self, builder: &mut RustwideBuilder) -> Result<bool> {
        let mut processed = false;
        self.process_next_crate(|krate| {
            processed = true;

            let kind = krate
                .registry
                .as_ref()
                .map(|r| PackageKind::Registry(r.as_str()))
                .unwrap_or(PackageKind::CratesIo);

            match retry(
                || {
                    builder
                        .update_toolchain()
                        .context("Updating toolchain failed, locking queue")
                },
                3,
            ) {
                Err(err) => {
                    report_error(&err);
                    self.lock()?;
                    return Err(err);
                }
                Ok(true) => {
                    // toolchain has changed, purge caches
                    if let Err(err) = retry(
                        || {
                            builder
                                .purge_caches()
                                .context("purging rustwide caches failed, locking queue")
                        },
                        3,
                    ) {
                        report_error(&err);
                        self.lock()?;
                        return Err(err);
                    }
                }
                Ok(false) => {}
            }

            builder.build_package(&krate.name, &krate.version, kind)?;
            Ok(())
        })?;

        Ok(processed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_add_duplicate_doesnt_fail_last_priority_wins() {
        crate::test::wrapper(|env| {
            let queue = env.build_queue();

            queue.add_crate("some_crate", "0.1.1", 0, None)?;
            queue.add_crate("some_crate", "0.1.1", 9, None)?;

            let queued_crates = queue.queued_crates()?;
            assert_eq!(queued_crates.len(), 1);
            assert_eq!(queued_crates[0].priority, 9);

            Ok(())
        })
    }

    #[test]
    fn test_add_duplicate_resets_attempts_and_priority() {
        crate::test::wrapper(|env| {
            env.override_config(|config| {
                config.build_attempts = 5;
            });

            let queue = env.build_queue();

            let mut conn = env.db().conn();
            conn.execute(
                "
                INSERT INTO queue (name, version, priority, attempt ) 
                VALUES ('failed_crate', '0.1.1', 0, 99)",
                &[],
            )?;

            assert_eq!(queue.pending_count()?, 0);

            queue.add_crate("failed_crate", "0.1.1", 9, None)?;

            assert_eq!(queue.pending_count()?, 1);

            let row = conn
                .query_opt(
                    "SELECT priority, attempt
                     FROM queue 
                     WHERE name = $1 AND version = $2",
                    &[&"failed_crate", &"0.1.1"],
                )?
                .unwrap();
            assert_eq!(row.get::<_, i32>(0), 9);
            assert_eq!(row.get::<_, i32>(1), 0);
            Ok(())
        })
    }

    #[test]
    fn test_add_and_process_crates() {
        const MAX_ATTEMPTS: u16 = 3;

        crate::test::wrapper(|env| {
            env.override_config(|config| {
                config.build_attempts = MAX_ATTEMPTS;
            });

            let queue = env.build_queue();

            let test_crates = [
                ("low-priority", "1.0.0", 1000),
                ("high-priority-foo", "1.0.0", -1000),
                ("medium-priority", "1.0.0", -10),
                ("high-priority-bar", "1.0.0", -1000),
                ("standard-priority", "1.0.0", 0),
                ("high-priority-baz", "1.0.0", -1000),
            ];
            for krate in &test_crates {
                queue.add_crate(krate.0, krate.1, krate.2, None)?;
            }

            let assert_next = |name| -> Result<()> {
                queue.process_next_crate(|krate| {
                    assert_eq!(name, krate.name);
                    Ok(())
                })?;
                Ok(())
            };
            let assert_next_and_fail = |name| -> Result<()> {
                queue.process_next_crate(|krate| {
                    assert_eq!(name, krate.name);
                    anyhow::bail!("simulate a failure");
                })?;
                Ok(())
            };

            // The first processed item is the one with the highest priority added first.
            assert_next("high-priority-foo")?;

            // Simulate a failure in high-priority-bar.
            assert_next_and_fail("high-priority-bar")?;

            // Continue with the next high priority crate.
            assert_next("high-priority-baz")?;

            // After all the crates with the max priority are processed, before starting to process
            // crates with a lower priority the failed crates with the max priority will be tried
            // again.
            assert_next("high-priority-bar")?;

            // Continue processing according to the priority.
            assert_next("medium-priority")?;
            assert_next("standard-priority")?;

            // Simulate the crate failing many times.
            for _ in 0..MAX_ATTEMPTS {
                assert_next_and_fail("low-priority")?;
            }

            // Since low-priority failed many times it will be removed from the queue. Because of
            // that the queue should now be empty.
            let mut called = false;
            queue.process_next_crate(|_| {
                called = true;
                Ok(())
            })?;
            assert!(!called, "there were still items in the queue");

            // Ensure metrics were recorded correctly
            let metrics = env.metrics();
            assert_eq!(metrics.total_builds.get(), 9);
            assert_eq!(metrics.failed_builds.get(), 1);

            Ok(())
        })
    }

    #[test]
    fn test_pending_count() {
        crate::test::wrapper(|env| {
            let queue = env.build_queue();

            assert_eq!(queue.pending_count()?, 0);
            queue.add_crate("foo", "1.0.0", 0, None)?;
            assert_eq!(queue.pending_count()?, 1);
            queue.add_crate("bar", "1.0.0", 0, None)?;
            assert_eq!(queue.pending_count()?, 2);

            queue.process_next_crate(|krate| {
                assert_eq!("foo", krate.name);
                Ok(())
            })?;
            assert_eq!(queue.pending_count()?, 1);

            Ok(())
        });
    }

    #[test]
    fn test_prioritized_count() {
        crate::test::wrapper(|env| {
            let queue = env.build_queue();

            assert_eq!(queue.prioritized_count()?, 0);
            queue.add_crate("foo", "1.0.0", 0, None)?;
            assert_eq!(queue.prioritized_count()?, 1);
            queue.add_crate("bar", "1.0.0", -100, None)?;
            assert_eq!(queue.prioritized_count()?, 2);
            queue.add_crate("baz", "1.0.0", 100, None)?;
            assert_eq!(queue.prioritized_count()?, 2);

            queue.process_next_crate(|krate| {
                assert_eq!("bar", krate.name);
                Ok(())
            })?;
            assert_eq!(queue.prioritized_count()?, 1);

            Ok(())
        });
    }

    #[test]
    fn test_failed_count() {
        const MAX_ATTEMPTS: u16 = 3;
        crate::test::wrapper(|env| {
            env.override_config(|config| {
                config.build_attempts = MAX_ATTEMPTS;
            });
            let queue = env.build_queue();

            assert_eq!(queue.failed_count()?, 0);
            queue.add_crate("foo", "1.0.0", -100, None)?;
            assert_eq!(queue.failed_count()?, 0);
            queue.add_crate("bar", "1.0.0", 0, None)?;

            for _ in 0..MAX_ATTEMPTS {
                assert_eq!(queue.failed_count()?, 0);
                queue.process_next_crate(|krate| {
                    assert_eq!("foo", krate.name);
                    anyhow::bail!("this failed");
                })?;
            }
            assert_eq!(queue.failed_count()?, 1);

            queue.process_next_crate(|krate| {
                assert_eq!("bar", krate.name);
                Ok(())
            })?;
            assert_eq!(queue.failed_count()?, 1);

            Ok(())
        });
    }

    #[test]
    fn test_queued_crates() {
        crate::test::wrapper(|env| {
            let queue = env.build_queue();

            let test_crates = [
                ("bar", "1.0.0", 0),
                ("foo", "1.0.0", -10),
                ("baz", "1.0.0", 10),
            ];
            for krate in &test_crates {
                queue.add_crate(krate.0, krate.1, krate.2, None)?;
            }

            assert_eq!(
                vec![
                    ("foo", "1.0.0", -10),
                    ("bar", "1.0.0", 0),
                    ("baz", "1.0.0", 10),
                ],
                queue
                    .queued_crates()?
                    .iter()
                    .map(|c| (c.name.as_str(), c.version.as_str(), c.priority))
                    .collect::<Vec<_>>()
            );

            Ok(())
        });
    }

    #[test]
    fn test_last_seen_reference_in_db() {
        crate::test::wrapper(|env| {
            let queue = env.build_queue();
            queue.unlock()?;
            assert!(!queue.is_locked()?);
            // initial db ref is empty
            assert_eq!(queue.last_seen_reference()?, None);
            assert!(!queue.is_locked()?);

            let oid = git2::Oid::from_str("ffffffff")?;
            queue.set_last_seen_reference(oid)?;

            assert_eq!(queue.last_seen_reference()?, Some(oid));
            assert!(!queue.is_locked()?);

            Ok(())
        });
    }

    #[test]
    fn test_broken_db_reference_breaks() {
        crate::test::wrapper(|env| {
            let mut conn = env.db().conn();
            set_config(&mut conn, ConfigName::LastSeenIndexReference, "invalid")?;

            let queue = env.build_queue();
            assert!(queue.last_seen_reference().is_err());

            Ok(())
        });
    }

    #[test]
    fn test_queue_lock() {
        crate::test::wrapper(|env| {
            let queue = env.build_queue();
            // unlocked without config
            assert!(!queue.is_locked()?);

            queue.lock()?;
            assert!(queue.is_locked()?);

            queue.unlock()?;
            assert!(!queue.is_locked()?);

            Ok(())
        });
    }
}
