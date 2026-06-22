use std::collections::BTreeMap;
use std::sync::{LazyLock, Mutex};
use std::time::{self, Duration};

use ansi_term::Style;
use dashmap::DashSet;
use rand::Rng;
use std::sync::Arc;
use tokio::task;

use crate::Options;
use crate::Plugin;
use crate::session::{Error, Session};

use super::plugin::PayloadStrategy;

type Inventory = BTreeMap<&'static str, Box<dyn Plugin>>;

macro_rules! register_plugin {
    ($($name:literal => $instance:expr),+) => {
        pub(crate) fn register(registrar: &mut impl $crate::plugins::manager::PluginRegistrar) {
            $(
                registrar.register($name, $instance);
            )*
        }
    };
}

pub(crate) use register_plugin;

pub(crate) trait PluginRegistrar {
    fn register<P: Plugin + 'static>(&mut self, name: &'static str, plugin: P);
}

impl PluginRegistrar for Inventory {
    #[inline]
    fn register<P: Plugin + 'static>(&mut self, name: &'static str, plugin: P) {
        self.insert(name, Box::new(plugin));
    }
}

pub(crate) static INVENTORY: LazyLock<Mutex<Inventory>> = LazyLock::new(|| {
    let mut ps = Inventory::new();
    super::add_defaults(&mut ps);
    Mutex::new(ps)
});

pub(crate) fn list() {
    let bold = Style::new().bold();

    println!("{}\n", bold.paint("Available plugins:"));

    let max_len = INVENTORY
        .lock()
        .unwrap()
        .keys()
        .map(|k| k.len())
        .max()
        .unwrap_or(0);

    for (key, plugin) in &*INVENTORY.lock().unwrap() {
        println!(
            "  {}{} : {}",
            bold.paint(*key),
            " ".repeat(max_len - key.len()), // padding
            plugin.description()
        );
    }
}

pub(crate) async fn setup(options: &Options) -> Result<&'static mut dyn Plugin, Error> {
    let Some(plugin_name) = options.plugin.as_ref() else {
        return Err("no plugin selected".to_owned());
    };
    let Some(plugin) = INVENTORY
        .lock()
        .unwrap()
        .remove(plugin_name.as_str())
        .map(Box::leak)
    else {
        return Err(format!(
            "{} is not a valid plugin name, run with --list-plugins to see the list of available plugins",
            plugin_name
        ));
    };

    plugin.setup(options).await?;

    Ok(plugin)
}

pub(crate) async fn run(
    plugin: &'static mut dyn Plugin,
    session: Arc<Session>,
) -> Result<(), Error> {
    let single = matches!(plugin.payload_strategy(), PayloadStrategy::Single);
    let override_payload = plugin.override_payload();
    let (combinations, restored_fully) = session.combinations(override_payload, single)?;
    if !restored_fully {
        log::debug!("restore was interrupted, exiting early");
        return Ok(());
    }
    let unreachables: Arc<DashSet<Arc<str>>> = Arc::new(DashSet::default());

    // spawn worker tasks
    for _ in 0..session.options.concurrency {
        task::spawn(worker(plugin, unreachables.clone(), session.clone()));
    }

    if !session.options.quiet {
        // start statistics reporting
        let stat_sess = session.clone();
        tokio::task::spawn(async move {
            stat_sess.report_runtime_statistics().await;
        });
    }

    let rate_limit = session.options.rate_limit;
    let cred_wait = if session.options.wait > 0 {
        Some(time::Duration::from_millis(session.options.wait as u64))
    } else {
        None
    };

    fn save_progress(session: &Session, dispatched: usize) {
        if dispatched > session.get_done() {
            session.done.store(dispatched, std::sync::atomic::Ordering::Relaxed);
        }
        if let Err(e) = session.save() {
            log::error!("could not save session: {:?}", e);
        }
    }

    // loop credentials for this session
    let mut dispatched: usize = 0;
    for creds in combinations {
        if session.is_stop() {
            log::debug!("exiting loop, saving progress at {}", dispatched);
            save_progress(&session, dispatched);
            return Ok(());
        }

        dispatched += 1;

        if rate_limit > 0 && dispatched.is_multiple_of(rate_limit) {
            tokio::time::sleep(time::Duration::from_secs(1)).await;
        }

        if let Some(wait) = cred_wait {
            tokio::time::sleep(wait).await;
        }

        if let Err(e) = session.send_credentials(creds).await {
            if session.is_stop() {
                log::debug!("{}", e);
                save_progress(&session, dispatched);
                return Ok(());
            } else {
                log::error!("{}", e);
                return Err(e);
            }
        }
    }

    Ok(())
}

fn normalize_target_key(target: &str) -> String {
    if target.starts_with('[') {
        if let Some(end_bracket) = target.find(']') {
            let ip = &target[1..end_bracket];
            let rest = &target[end_bracket + 1..];
            return format!("{}{}", ip, rest);
        }
    }
    target.to_string()
}

async fn worker(plugin: &dyn Plugin, unreachables: Arc<DashSet<Arc<str>>>, session: Arc<Session>) {
    log::debug!("worker started");

    let retries = session.options.retries;
    let retry_time: time::Duration = time::Duration::from_millis(session.options.retry_time);
    let has_jitter = session.options.jitter_max > 0;
    let jitter_min = session.options.jitter_min;
    let jitter_max = session.options.jitter_max;
    let has_placeholders = |target: &str| {
        target.contains('{') && target.contains('}')
    };

    while let Ok(creds) = session.recv_credentials().await {
        if session.is_stop() {
            log::debug!("exiting worker");
            break;
        }

        let target_key = normalize_target_key(&creds.target);
        let has_target_placeholders = has_placeholders(&creds.target);
        let is_unreachable = !has_target_placeholders && unreachables.contains(target_key.as_str());

        if is_unreachable {
            session.inc_done();
            continue;
        }

        let mut attempted = false;
        let mut all_attempts_failed = false;

        for attempt_num in 1..=retries {
            if session.is_stop() {
                break;
            }

            if has_jitter {
                let ms = rand::rng().random_range(jitter_min..=jitter_max);
                if ms > 0 {
                    log::debug!("jitter of {} ms", ms);
                    tokio::time::sleep(time::Duration::from_millis(ms)).await;
                }
            }

            if session.is_stop() {
                break;
            }

            attempted = true;
            let timeout = session.runtime.get_timeout();

            match plugin.attempt(&creds, timeout).await {
                Err(err) => {
                    if attempt_num < retries {
                        log::debug!(
                            "[{}] attempt {}/{}: {}",
                            &creds.target,
                            attempt_num,
                            retries,
                            err
                        );
                        tokio::time::sleep(retry_time).await;
                    } else {
                        all_attempts_failed = true;
                        if !has_target_placeholders {
                            unreachables.insert(Arc::from(target_key.as_str()));
                        }
                        log::error!(
                            "[{}] attempt {}/{}: {}",
                            &creds.target,
                            attempt_num,
                            retries,
                            err
                        );
                    }
                }
                Ok(loot) => {
                    if let Some(loots) = loot {
                        for loot in loots {
                            if let Some(mut elapsed) = loot.get_elapsed_time() {
                                if elapsed.as_millis() == 0 {
                                    elapsed = Duration::from_millis(1);
                                }
                                elapsed *= 10;
                                if elapsed < timeout {
                                    session.runtime.set_timeout(elapsed.as_millis() as u64);
                                }
                            }

                            session.add_loot(loot).await.unwrap();
                        }
                    }
                    break;
                }
            }
        }

        if attempted {
            session.inc_done();
            if all_attempts_failed {
                session.inc_errors();
                log::debug!("retries={} all failed", retries);
            }
        }
    }

    log::debug!("worker exit");
}

#[cfg(test)]
#[path = "manager_test.rs"]
mod manager_test;
