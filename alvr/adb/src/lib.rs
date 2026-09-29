pub mod commands;
mod parse;

use alvr_common::anyhow::Result;
use alvr_common::{dbg_connection, warn};
use alvr_session::WiredClientAutoLaunchConfig;
use alvr_system_info::{
    ClientFlavor, PACKAGE_NAME_GITHUB_DEV, PACKAGE_NAME_GITHUB_STABLE, PACKAGE_NAME_STORE,
};
use std::collections::HashSet;
use std::time::Duration;

pub enum WiredConnectionStatus {
    Ready,
    NotReady(String),
}

pub struct WiredConnection {
    adb_path: String,
}

impl WiredConnection {
    pub fn new(
        layout: &alvr_filesystem::Layout,
        download_progress_callback: impl Fn(usize, Option<usize>),
    ) -> Result<Self> {
        let adb_path = commands::require_adb(layout, download_progress_callback)?;

        Ok(Self { adb_path })
    }

    /// control_port is (local PC port, headset port); the stream port is the same on both ends.
    pub fn setup(
        &self,
        control_port: (u16, u16),
        stream_port: u16,
        client_type: &ClientFlavor,
        client_autolaunch: Option<WiredClientAutoLaunchConfig>,
    ) -> Result<WiredConnectionStatus> {
        // ALVR_WIRED_SERIAL pins this runtime instance to one headset (one instance per headset).
        let pinned = std::env::var("ALVR_WIRED_SERIAL").ok().filter(|s| !s.is_empty());
        let device_serials: Vec<String> = commands::list_devices(&self.adb_path)?
            .into_iter()
            .filter_map(|d| d.serial)
            .filter(|s| !s.starts_with("127.0.0.1"))
            .filter(|s| pinned.as_ref().is_none_or(|p| p == s))
            .collect();
        if device_serials.is_empty() {
            return Ok(WiredConnectionStatus::NotReady(
                "No wired devices found".to_owned(),
            ));
        }

        // Several devices may be attached (e.g. a phone next to the headset): use the
        // first one that has a suitable ALVR client installed.
        let Some((device_serial, process_name)) = device_serials.into_iter().find_map(|serial| {
            get_process_name(&self.adb_path, &serial, client_type).map(|name| (serial, name))
        }) else {
            return Ok(WiredConnectionStatus::NotReady(
                "No suitable ALVR client is installed".to_owned(),
            ));
        };

        let wanted = [control_port, (stream_port, stream_port)];
        let forwarded: HashSet<(u16, u16)> =
            commands::list_forwarded_ports(&self.adb_path, &device_serial)?
                .into_iter()
                .map(|f| (f.local, f.remote))
                .collect();
        for (local, remote) in wanted.into_iter().filter(|p| !forwarded.contains(p)) {
            commands::forward_port(&self.adb_path, &device_serial, local, remote)?;
            dbg_connection!(
                "setup_wired_connection: Forwarded port {local} -> {remote} of device {device_serial}"
            );
        }

        if commands::get_process_id(&self.adb_path, &device_serial, &process_name)?.is_none() {
            if let Some(client_autolaunch) = client_autolaunch {
                if client_autolaunch.boot_delay > 0 {
                    match commands::get_uptime(&self.adb_path, &device_serial) {
                        Ok(uptime) => {
                            if uptime < Duration::from_secs(client_autolaunch.boot_delay.into()) {
                                return Ok(WiredConnectionStatus::NotReady(
                                    "Waiting for device boot".to_owned(),
                                ));
                            }
                        }
                        Err(failure) => {
                            warn!("wired_connection: get_uptime failed with {}", failure);
                        }
                    }
                }

                commands::start_application(&self.adb_path, &device_serial, &process_name)?;
                Ok(WiredConnectionStatus::NotReady(
                    "Starting ALVR client".to_owned(),
                ))
            } else {
                Ok(WiredConnectionStatus::NotReady(
                    "ALVR client is not running".to_owned(),
                ))
            }
        } else if !commands::is_activity_resumed(&self.adb_path, &device_serial, &process_name)? {
            Ok(WiredConnectionStatus::NotReady(
                "ALVR client is paused".to_owned(),
            ))
        } else {
            Ok(WiredConnectionStatus::Ready)
        }
    }
}

// ADB is a shared host service used by the panel, tablets and other runtime instances.
// Dropping this connection must not kill it or remove other clients' forwards.
// Our forwarding setup is idempotent and checks the owning device on the next connection.

#[cfg(all(test, unix))]
mod lifecycle_tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt};

    #[test]
    fn dropping_wired_connection_does_not_execute_adb() {
        let dir = std::env::temp_dir().join(format!("alvr-adb-drop-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let adb = dir.join("adb");
        let marker = dir.join("called");
        fs::write(&adb, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
        fs::set_permissions(&adb, fs::Permissions::from_mode(0o700)).unwrap();
        drop(WiredConnection { adb_path: adb.to_string_lossy().into_owned() });
        let executed = marker.exists();
        fs::remove_dir_all(&dir).unwrap();
        assert!(!executed, "connection teardown must leave the shared ADB server alive");
    }
}

pub fn get_process_name(
    adb_path: &str,
    device_serial: &str,
    flavor: &ClientFlavor,
) -> Option<String> {
    let fallbacks = match flavor {
        ClientFlavor::Store => {
            if alvr_common::is_stable() {
                vec![PACKAGE_NAME_STORE, PACKAGE_NAME_GITHUB_STABLE]
            } else {
                vec![PACKAGE_NAME_GITHUB_DEV]
            }
        }
        ClientFlavor::Github => {
            if alvr_common::is_stable() {
                vec![PACKAGE_NAME_GITHUB_STABLE, PACKAGE_NAME_STORE]
            } else {
                vec![PACKAGE_NAME_GITHUB_DEV]
            }
        }
        ClientFlavor::Custom(name) => {
            if alvr_common::is_stable() {
                vec![name, PACKAGE_NAME_STORE, PACKAGE_NAME_GITHUB_STABLE]
            } else {
                vec![name, PACKAGE_NAME_GITHUB_DEV]
            }
        }
    };

    fallbacks
        .iter()
        .find(|name| {
            commands::is_package_installed(adb_path, device_serial, name)
                .is_ok_and(|installed| installed)
        })
        .map(|name| (*name).to_string())
}
