use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use human_bytes::human_bytes;
use memory_stats::memory_stats;
use rand::Rng;
use serde::{Deserialize, Serialize};

use crate::Options;
use crate::creds::{Combinator, Expression};

pub(crate) mod loot;
mod runtime;

use runtime::*;

pub(crate) use crate::Credentials;
use crate::utils::{parse_multiple_targets, parse_target};
pub(crate) use loot::Loot;

use std::sync::{Arc, Mutex};
use std::time;

pub(crate) type Error = String;

async fn periodic_saver(session: Arc<Session>) {
    let report_interval = time::Duration::from_millis(session.options.report_time);
    let mut last_done: usize = 0;
    let persistent = session.options.session.is_some();

    while !session.is_stop() {
        tokio::time::sleep(report_interval).await;

        // compute number of attempts per second
        let new_done = session.get_done();
        let speed = (new_done - last_done) as f64 / report_interval.as_secs_f64();
        last_done = new_done;

        session.set_speed(speed as usize);

        if persistent && let Err(e) = session.save() {
            log::error!("could not save session: {:?}", e);
        }
    }

    // update and save to the last state before exiting
    if persistent && let Err(e) = session.save() {
        log::error!("could not save session: {:?}", e);
    }
}

#[derive(Serialize, Deserialize, Debug, Default)]
pub(crate) struct Statistics {
    tasks: usize,
    memory: f64,
    targets: usize,
    attempts: usize,
    done: usize,
    done_percent: f32,
    errors: usize,
    reqs_per_sec: usize,
    timeout: u64,
}

impl Statistics {
    pub fn to_text(&self) -> String {
        if self.errors > 0 {
            format!(
                "tasks={} mem={} targets={} attempts={} done={} ({:.2?}%) errors={} timeout={}ms speed={:.2?} reqs/s",
                self.tasks,
                human_bytes(self.memory),
                self.targets,
                self.attempts,
                self.done,
                self.done_percent,
                self.errors,
                self.timeout,
                self.reqs_per_sec,
            )
        } else {
            format!(
                "tasks={} mem={} targets={} attempts={} done={} ({:.2?}%) timeout={}ms speed={:.2?} reqs/s",
                self.tasks,
                human_bytes(self.memory),
                self.targets,
                self.attempts,
                self.done,
                self.done_percent,
                self.timeout,
                self.reqs_per_sec,
            )
        }
    }

    pub fn to_json(&self) -> Result<String, Error> {
        serde_json::to_string(self).map_err(|e| e.to_string())
    }

    pub fn update_from_json(&mut self, json: &str) -> Result<(), Error> {
        *self = serde_json::from_str(json).map_err(|e| e.to_string())?;
        Ok(())
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct Session {
    pub options: Options,
    pub targets: Vec<String>,
    pub total: AtomicUsize,
    pub done: AtomicUsize,
    pub errors: AtomicUsize,
    pub results: Mutex<Vec<Loot>>,

    #[serde(skip_serializing, skip_deserializing)]
    pub runtime: Runtime,
}

impl Session {
    fn from_options(options: Options) -> Result<Arc<Self>, Error> {
        let targets = if let Some(target) = options.target.as_ref() {
            parse_multiple_targets(target)?
        } else {
            return Err("no --target/-T argument provided".to_owned());
        };

        if targets.is_empty() {
            return Err("empty list of target(s) provided".to_owned());
        }

        // perform pre-emptive target validation
        for target in &targets {
            parse_target(target, 0)?;
        }

        let runtime = Runtime::new(options.concurrency, options.timeout);
        let total = AtomicUsize::new(0);
        let done = AtomicUsize::new(0);
        let errors = AtomicUsize::new(0);
        let results = Mutex::new(vec![]);

        Ok(Arc::new(Self {
            options,
            targets,
            total,
            done,
            errors,
            results,
            runtime,
        }))
    }

    fn from_disk(path: &str, options: Options) -> Result<Arc<Self>, Error> {
        if Path::new(path).exists() {
            log::info!("restoring session from {}", path);

            let file = fs::File::open(path).map_err(|e| e.to_string())?;
            let mut session: Session = serde_json::from_reader(file).map_err(|e| e.to_string())?;

            session.options = options.clone();
            session.runtime = Runtime::new(options.concurrency, options.timeout);

            if options.single_match {
                if let Ok(results) = session.results.lock() {
                    if !results.is_empty() {
                        let total = session.get_total();
                        session.done.store(total, Ordering::Relaxed);
                        log::info!("single-match mode with existing loot, marking as complete");
                    }
                }
            }

            Ok(Arc::new(session))
        } else {
            Self::from_options(options)
        }
    }

    pub fn new(options: Options) -> Result<Arc<Self>, Error> {
        // if a session file has been specified
        let session = if let Some(path) = options.session.as_ref() {
            // load from disk if file exists, or from options and save to disk
            Self::from_disk(path, options.clone())?
        } else {
            // create new without persistency
            Self::from_options(options)?
        };

        let num_targets = session.targets.len();
        log::info!(
            "target{}: {}",
            if num_targets > 1 {
                format!("s ({})", num_targets)
            } else {
                "".to_owned()
            },
            session.options.target.as_ref().unwrap()
        );

        // set ctrl-c handler
        let le_session = session.clone();
        ctrlc::set_handler(move || {
            // avoid triggering this if ctrl-c has been already triggered
            if !le_session.is_stop() {
                log::info!("stopping ...");
                le_session.set_stop();
            }
        })
        .expect("error setting ctrl-c handler");

        tokio::task::spawn(periodic_saver(session.clone()));

        Ok(session)
    }

    #[cfg(test)]
    pub fn new_for_tests(options: Options) -> Result<Arc<Self>, Error> {
        // if a session file has been specified
        let session = if let Some(path) = options.session.as_ref() {
            // load from disk if file exists, or from options and save to disk
            Self::from_disk(path, options.clone())?
        } else {
            // create new without persistency
            Self::from_options(options)?
        };

        // Don't set ctrl-c handler in tests and don't spawn periodic saver
        Ok(session)
    }

    pub fn is_stop(&self) -> bool {
        self.runtime.is_stop()
    }

    pub fn set_stop(&self) {
        self.runtime.set_stop()
    }

    pub fn set_speed(&self, rps: usize) {
        self.runtime.set_speed(rps);
    }

    pub fn get_speed(&self) -> usize {
        self.runtime.get_speed()
    }

    pub async fn send_credentials(&self, creds: Credentials) -> Result<(), Error> {
        self.runtime.send_credentials(creds).await
    }

    pub async fn recv_credentials(&self) -> Result<Credentials, Error> {
        self.runtime.recv_credentials().await
    }

    pub fn is_done(&self) -> bool {
        self.get_done() >= self.get_total()
    }

    pub fn is_finished(&self) -> bool {
        self.is_done() || self.is_stop()
    }

    pub fn inc_errors(&self) {
        self.errors.fetch_add(1, Ordering::Relaxed);
    }

    pub fn get_errors(&self) -> usize {
        self.errors.load(Ordering::Relaxed)
    }

    pub fn inc_done(&self) {
        self.done.fetch_add(1, Ordering::Relaxed);
    }

    pub fn get_done(&self) -> usize {
        self.done.load(Ordering::Relaxed)
    }

    pub fn set_total(&self, value: usize) {
        self.total.store(value, Ordering::Relaxed);
    }

    pub fn get_total(&self) -> usize {
        self.total.load(Ordering::Relaxed)
    }

    pub fn combinations(
        &self,
        override_payload: Option<Expression>,
        single: bool,
    ) -> Result<(Combinator, bool), Error> {
        let sess = self.clone();
        let (combinator, restored_fully) = Combinator::create(
            &self.targets,
            self.options.clone(),
            self.get_done(),
            single,
            override_payload,
            move || sess.is_stop(),
        )?;

        self.set_total(combinator.search_space_size());

        if !restored_fully {
            if let Err(e) = self.save() {
                log::error!("could not save session after interrupted restore: {:?}", e);
            }
        }

        if single {
            log::info!("using -> {}\n", combinator.username_expression());
        } else {
            log::info!("username -> {}", combinator.username_expression());
            log::info!("password -> {}\n", combinator.password_expression());
        }

        Ok((combinator, restored_fully))
    }

    pub async fn add_loot(&self, loot: Loot) -> Result<(), Error> {
        // append to loot vector
        if let Ok(mut results) = self.results.lock() {
            if !results.contains(&loot) {
                results.push(loot.clone());

                // report credentials to screen
                if self.options.json {
                    println!("{}", loot.to_json().unwrap());
                } else {
                    log::info!("{}", &loot);
                }

                // check if we have to output to file
                if let Some(path) = &self.options.output
                    && let Err(e) = loot.append_to_file(path, &self.options.output_format)
                {
                    log::error!("could not write to {}: {:?}", &path, e);
                }

                // if we only need one match, stop
                if !loot.is_partial() && self.options.single_match {
                    self.set_stop();
                }

                // save session if needed
                return self.save();
            }
        } else {
            return Err("could not lock session results".to_owned());
        }

        Ok(())
    }

    pub fn save(&self) -> Result<(), Error> {
        if let Some(path) = self.options.session.as_ref() {
            log::debug!("saving session to {}", path);
            let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;

            let pid = std::process::id();
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis();
            let rand: u64 = rand::rng().random();
            let tmp_path = format!("{}.{}.{}.{}.tmp", path, pid, now, rand);

            let mut file = fs::File::create(&tmp_path).map_err(|e| e.to_string())?;
            file.write_all(json.as_bytes()).map_err(|e| e.to_string())?;
            file.sync_all().map_err(|e| e.to_string())?;
            drop(file);

            fs::rename(&tmp_path, path).map_err(|e| {
                let _ = fs::remove_file(&tmp_path);
                e.to_string()
            })?;
        }
        Ok(())
    }

    pub async fn report_runtime_statistics(&self) {
        let report_interval = time::Duration::from_millis(self.options.report_time);
        while !self.is_stop() {
            let total = self.get_total();
            let done = self.get_done();
            let perc = (done as f32 / total as f32) * 100.0;
            let errors = self.get_errors();
            let speed: usize = self.get_speed();
            let memory = if let Some(usage) = memory_stats() {
                usage.physical_mem
            } else {
                log::error!("couldn't get the current memory usage");
                0
            };

            let stats = Statistics {
                tasks: self.options.concurrency,
                memory: memory as f64,
                targets: self.targets.len(),
                attempts: total,
                done,
                done_percent: perc,
                errors,
                reqs_per_sec: speed,
                timeout: self.runtime.get_timeout_ms(),
            };

            if self.options.json {
                println!("{}", stats.to_json().unwrap());
            } else {
                log::info!("{}", stats.to_text());
            }

            tokio::time::sleep(report_interval).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Options;
    use std::fs;
    use std::sync::atomic::Ordering;

    fn make_options(target: &str, retries: usize, single_match: bool) -> Options {
        Options {
            target: Some(target.to_owned()),
            username: Some("u".to_owned()),
            password: Some("p".to_owned()),
            retries,
            single_match,
            concurrency: 1,
            timeout: 1000,
            quiet: true,
            ..Default::default()
        }
    }

    #[test]
    fn new_options_override_disk_session_options() {
        let tmpdir = tempfile::tempdir().unwrap();
        let session_path = tmpdir.path().join("session.json");

        let opts_old = make_options("127.0.0.1:80", 1, false);
        let opts_old_with_session = Options {
            session: Some(session_path.to_str().unwrap().to_owned()),
            ..opts_old
        };
        {
            let sess = Session::new_for_tests(opts_old_with_session).unwrap();
            sess.save().unwrap();
        }

        let opts_new = make_options("127.0.0.1:80", 99, false);
        let opts_new_with_session = Options {
            session: Some(session_path.to_str().unwrap().to_owned()),
            ..opts_new
        };
        let restored = Session::new_for_tests(opts_new_with_session).unwrap();

        assert_eq!(restored.options.retries, 99);
    }

    #[test]
    fn single_match_with_existing_loot_marks_complete() {
        let tmpdir = tempfile::tempdir().unwrap();
        let session_path = tmpdir.path().join("session.json");

        let opts = make_options("127.0.0.1:80", 3, true);
        let opts_with_session = Options {
            session: Some(session_path.to_str().unwrap().to_owned()),
            ..opts
        };
        {
            let sess = Session::new_for_tests(opts_with_session).unwrap();
            sess.set_total(100);
            sess.done.store(5, Ordering::Relaxed);
            sess.results.lock().unwrap().push(Loot::new(
                "test",
                "127.0.0.1:80",
                vec![
                    ("username".to_string(), "u".to_string()),
                    ("password".to_string(), "p".to_string()),
                ],
            ));
            sess.save().unwrap();
        }

        let opts2 = make_options("127.0.0.1:80", 3, true);
        let opts2_with_session = Options {
            session: Some(session_path.to_str().unwrap().to_owned()),
            ..opts2
        };
        let restored = Session::new_for_tests(opts2_with_session).unwrap();

        assert_eq!(restored.get_done(), restored.get_total());
        assert!(restored.is_done());
    }

    #[test]
    fn concurrent_saves_do_not_corrupt() {
        let tmpdir = tempfile::tempdir().unwrap();
        let session_path = tmpdir.path().join("session.json");

        let opts = make_options("127.0.0.1:80", 3, false);
        let opts_with_session = Options {
            session: Some(session_path.to_str().unwrap().to_owned()),
            ..opts
        };
        let sess = Session::new_for_tests(opts_with_session).unwrap();
        sess.set_total(10);

        let mut handles = vec![];
        for i in 0..10 {
            let s = sess.clone();
            handles.push(std::thread::spawn(move || {
                s.done.store(i, Ordering::Relaxed);
                s.save().unwrap();
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        drop(sess);

        let content = fs::read_to_string(&session_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert!(parsed.is_object());
        assert!(parsed.get("done").is_some());
    }

    #[test]
    fn save_uses_unique_temp_files() {
        let tmpdir = tempfile::tempdir().unwrap();
        let session_path = tmpdir.path().join("session.json");

        let opts = make_options("127.0.0.1:80", 3, false);
        let opts_with_session = Options {
            session: Some(session_path.to_str().unwrap().to_owned()),
            ..opts
        };
        let sess = Session::new_for_tests(opts_with_session).unwrap();

        sess.save().unwrap();
        sess.save().unwrap();

        let mut tmps = vec![];
        for entry in fs::read_dir(tmpdir.path()).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().into_string().unwrap();
            if name.ends_with(".tmp") {
                tmps.push(name);
            }
        }
        assert!(tmps.is_empty(), "leftover tmp files: {:?}", tmps);
        assert!(session_path.exists());
    }
}
