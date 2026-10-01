//! Fixed demonstration data, compiled only into demo builds.
use super::*;
use serialcore::config::{ExtractMode, TimestampFormat};

impl App {
    pub(super) fn seed_demo(&mut self) -> std::io::Result<()> {
        self.demo_mode = true;
        self.config.settings.theme = "dark".into();
        self.config.settings.console_font_size = 16;
        self.config.settings.timestamp_format = TimestampFormat::Time;
        self.config.settings.check_updates = false;

        let mut samples = vec![
            "Device 1 initialized".into(),
            "[info] Configuration loaded".into(),
            "[info] Sensor interface ready".into(),
            "[info] Calibration loaded".into(),
            "[info] Communication link established".into(),
            "\x1b[96mdevice 1>\x1b[0m sensor stream 84".into(),
        ];
        // Enough history to produce useful curves, with a short readable tail.
        for i in 0..90 {
            let t = i as f64;
            samples.push(format!(
                "\x1b[92msensor\x1b[0m: temperature={:.2} humidity={:.2}",
                23.4 + (t / 12.0).sin() * 0.8 + t * 0.008,
                45.0 + (t / 17.0).cos() * 2.5,
            ));
        }
        // Pause between finite captures to inspect the device, then resume.
        let tail = samples.split_off(samples.len() - 6);
        samples.push("[info] Capture complete: 84 samples".into());
        samples.push("\x1b[96mdevice 1>\x1b[0m device status".into());
        samples.extend([
            "  State: ready".into(),
            "  Link: connected".into(),
            "  Samples captured: 84".into(),
            "  Dropped samples: 0".into(),
            "\x1b[96mdevice 1>\x1b[0m sensor status".into(),
            "  \x1b[92msensor\x1b[0m: ready; temperature and humidity enabled".into(),
            "  Sample interval: 1000 ms".into(),
            "\x1b[96mdevice 1>\x1b[0m sensor stream 6".into(),
        ]);
        samples.extend(tail);
        samples.push("[info] Capture complete: 6 samples; total: 90".into());
        samples.push("\x1b[96mdevice 1>\x1b[0m ".into());
        self.add_demo_console("device 1", samples, true)?;

        self.add_demo_console(
            "device 2",
            [
                "Device 2 initialized",
                "[info] Loading configuration",
                "[info] Configuration loaded",
                "[info] Communication interface ready",
                "[info] Self-test complete",
                "[info] Monitoring enabled",
                "",
                "\x1b[96mdevice 2>\x1b[0m device status",
                "  State: ready",
                "  Link: connected",
                "  Samples captured: 90",
                "  Dropped samples: 0",
                "",
                "\x1b[96mdevice 2>\x1b[0m sensor read",
                "\x1b[92msensor\x1b[0m: temperature=24.84 humidity=46.25",
                "",
                "\x1b[96mdevice 2>\x1b[0m link status",
                "  Peer: device 1",
                "  Signal: good",
                "  Last sample: received",
                "\x1b[96mdevice 2>\x1b[0m ",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
            false,
        )?;
        self.active = 0;
        self.next_port_id = 2;
        Ok(())
    }

    fn add_demo_console(
        &mut self,
        label: &str,
        lines: Vec<String>,
        plot: bool,
    ) -> std::io::Result<()> {
        let id = PortId(self.connections.len() as u32);
        let identity = PortIdentity {
            path_fallback: format!("demo:{}", id.0),
            ..Default::default()
        };
        let port_config = PortConfig::default();
        let mut handle = reader::spawn(
            reader::ReaderConfig {
                port_id: id,
                clock: self.clock.clone(),
                session_dir: None,
                meta: SessionMeta {
                    identity: identity.clone(),
                    config: port_config.clone(),
                    start_wall: self.clock.start_wall(),
                    app_version: env!("CARGO_PKG_VERSION").into(),
                    port_label: label.into(),
                    cleared: false,
                },
                terminal: port_config.terminal,
                wake: Wake::new(|| {}),
            },
            SourceSpec::OneShot(Box::new(
                serialcore::source::ScriptedSource::new(Vec::new()),
            )),
        )?;
        // No live thread or pending state events can change the scene.
        handle.shutdown_in_place();
        let mut conn = self.make_connection(id, label.into(), identity, port_config, handle);
        conn.state = ConnState::Connected;
        // End both captures at 13:37 local time, independent of launch time.
        let end_wall = self
            .clock
            .start_wall()
            .with_timezone(&chrono::Local)
            .date_naive()
            .and_hms_opt(13, 37, 0)
            .expect("valid demo time")
            .and_local_timezone(chrono::Local)
            .earliest()
            .expect("local demo time")
            .with_timezone(&chrono::Utc);
        let start_wall = end_wall - chrono::Duration::seconds(lines.len() as i64 - 1);
        conn.open_live_raw_session();
        let mut framer = Framer::with_mode(conn.port_config.terminal);
        let mut framed = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            let bytes = format!("{line}\r\n");
            conn.push_raw_bytes(bytes.as_bytes());
            framer.push(
                bytes.as_bytes(),
                Timestamp {
                    wall: start_wall + chrono::Duration::seconds(i as i64),
                    micros: i as i64 * 1_000_000,
                },
                &mut framed,
            );
        }
        for line in framed {
            let styled = serialcore::ansi::parse_line(&line.text, line.cursor);
            conn.store.append(IncomingLine {
                text: styled.text,
                ts: line.ts,
                port: id,
                flags: line.flags,
                spans: styled.spans,
                cursor: styled.cursor.map(|c| c as u32),
            });
        }
        if plot {
            conn.extract_rules.push(ExtractRule {
                mode: ExtractMode::Kv,
                prefix: Some("sensor:".into()),
                pattern: None,
                kv_separators: None,
            });
            conn.extract_dirty = true;
            conn.maintain_extract();
            if let Some(humidity) = conn
                .series
                .iter_mut()
                .find(|e| e.series.name() == "humidity")
            {
                humidity.own_axis = true;
            }
            conn.show_plot = true;
            conn.plot_fit = true;
        }
        self.tab_order.push(TabId::Connection(id));
        self.connections.push(conn);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_hex_view_renders_sample_bytes_for_both_devices() {
        let dir = tempfile::tempdir().unwrap();
        let paths = AppPaths {
            config_file: dir.path().join("pigtail.toml"),
            sessions: dir.path().join("sessions"),
            crash_log: dir.path().join("crash.log"),
        };
        let (_, rx) = crossbeam_channel::unbounded();
        let mut app = App::assemble(Config::default(), paths, Wake::new(|| {}), rx);
        app.seed_demo().unwrap();

        for active in 0..2 {
            app.active = active;
            app.connections[active].hex_view = true;
            app.connections[active].follow = false;
            let ctx = egui::Context::default();
            let output = ctx.run(egui::RawInput::default(), |ctx| {
                app.show_console(ctx, false);
            });
            assert!(
                output.shapes.iter().any(|shape| {
                    matches!(&shape.shape, egui::Shape::Text(text)
                    if text.galley.text().contains("00000000"))
                }),
                "device {} has no hex rows",
                active + 1
            );
        }
    }

    #[test]
    fn demo_rejects_file_transfers_instead_of_waiting_for_a_dead_reader() {
        let dir = tempfile::tempdir().unwrap();
        let paths = AppPaths {
            config_file: dir.path().join("pigtail.toml"),
            sessions: dir.path().join("sessions"),
            crash_log: dir.path().join("crash.log"),
        };
        let (_, rx) = crossbeam_channel::unbounded();
        let mut app = App::assemble(Config::default(), paths, Wake::new(|| {}), rx);
        app.seed_demo().unwrap();
        let file = dir.path().join("send.txt");
        std::fs::write(&file, "sample data").unwrap();

        let ctx = egui::Context::default();
        let _ = ctx.run(
            egui::RawInput {
                dropped_files: vec![egui::DroppedFile {
                    path: Some(file),
                    ..Default::default()
                }],
                ..Default::default()
            },
            |ctx| app.poll_file_drop(ctx),
        );
        assert!(app.file_transfer_dialog.is_none());
        assert!(app
            .connections
            .iter()
            .all(|conn| conn.transfer_progress.is_none()));
        assert!(!app.connect_errors.is_empty());
    }

    #[test]
    fn scene_has_colored_history_plots_and_never_persists_config() {
        let dir = tempfile::tempdir().unwrap();
        let paths = AppPaths {
            config_file: dir.path().join("pigtail.toml"),
            sessions: dir.path().join("sessions"),
            crash_log: dir.path().join("crash.log"),
        };
        let (_, rx) = crossbeam_channel::unbounded();
        let mut app = App::assemble(Config::default(), paths, Wake::new(|| {}), rx);
        app.seed_demo().unwrap();

        assert_eq!(app.connections.len(), 2);
        assert_eq!(app.config.settings.timestamp_format, TimestampFormat::Time);
        assert!(app.available.is_empty());
        assert!(!app.config.settings.check_updates);
        let conn = &app.connections[0];
        assert_eq!(conn.state, ConnState::Connected);
        assert!(conn.show_plot);
        assert_eq!(conn.series.len(), 2);
        assert!(conn.series.iter().all(|e| e.series.len() == 90));
        assert!(conn.series.iter().any(|e| e.own_axis));
        assert!(conn
            .store
            .get(6)
            .unwrap()
            .meta
            .spans
            .iter()
            .any(|s| s.rgb != serialcore::store::ColorSpan::NO_COLOR));
        assert!(conn.raw_ring.contains(&0x1b));
        assert!(!app.connections[0].drain_events(1_000_000));
        assert_eq!(app.connections[0].state, ConnState::Connected);

        app.save_session();
        assert!(app.flush_config());
        assert!(!app.paths.config_file.exists());
        assert!(!app.paths.sessions.exists());
    }

    #[test]
    fn demo_reconnect_does_not_start_a_serial_reader_or_create_captures() {
        let dir = tempfile::tempdir().unwrap();
        let paths = AppPaths {
            config_file: dir.path().join("pigtail.toml"),
            sessions: dir.path().join("sessions"),
            crash_log: dir.path().join("crash.log"),
        };
        let (_, rx) = crossbeam_channel::unbounded();
        let mut app = App::assemble(Config::default(), paths, Wake::new(|| {}), rx);
        app.seed_demo().unwrap();
        let id = app.connections[0].id;
        let raw = app.connections[0].raw_ring.clone();

        app.reconnect_with_config(id, None, PortConfig::default());

        assert_eq!(app.connections[0].state, ConnState::Connected);
        assert_eq!(app.connections[0].raw_ring, raw);
        assert!(!app.connections[0].drain_events(1_000_000));
        assert!(!app.connect_errors.is_empty());
        let identity = app.connections[0].identity.clone();
        assert!(matches!(
            app.spawn_serial_reader(id, &identity, &PortConfig::default(), None),
            Err(e) if e.kind() == std::io::ErrorKind::Unsupported
        ));
        app.open_connection(identity, None, PortConfig::default());
        assert_eq!(app.connections.len(), 2);
        assert!(!app.paths.sessions.exists());
    }

    #[test]
    fn demo_cannot_check_for_or_download_a_normal_app_update() {
        let dir = tempfile::tempdir().unwrap();
        let paths = AppPaths {
            config_file: dir.path().join("pigtail.toml"),
            sessions: dir.path().join("sessions"),
            crash_log: dir.path().join("crash.log"),
        };
        let (_, rx) = crossbeam_channel::unbounded();
        let mut app = App::assemble(Config::default(), paths, Wake::new(|| {}), rx);
        app.seed_demo().unwrap();

        // Even enabling the preference or calling the manual action must not
        // launch an updater that could replace the demo with a normal build.
        app.config.settings.check_updates = true;
        app.start_update_check(false);
        assert!(app.update_rx.is_none());
        app.start_update_check(true);
        assert!(app.update_rx.is_none());
        app.start_update_download("999.0.0".into());
        assert!(app.install_rx.is_none());
        assert!(app.update_progress.is_none());
        assert!(app.update_dialog.is_none());
    }
}
