//! `aiframes --selftest [--keep] [--dir <folder>]`: an end-to-end smoke test of the app's paths
//! without a window: generate media with ffmpeg (lavfi), build and save a project, reopen it,
//! render frames through the playback engine (paused + 2 s of real-time playback), apply/undo
//! ops, export 2 s, and print a pass/fail report. Exit code 0 = all passed.

use crate::playback::{Engine, Snapshot};
use aiframes_core::{Document, Op};
use aiframes_mcp::{actions, suite_hub::lock, JobStatus, ProjectDoc, Services};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

struct Report {
    steps: Vec<(String, bool, String, f64)>,
}

impl Report {
    fn step<T>(&mut self, name: &str, f: impl FnOnce() -> Result<(T, String), String>) -> Option<T> {
        let t0 = Instant::now();
        let r = f();
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        let (ok, detail, v) = match r {
            Ok((v, d)) => (true, d, Some(v)),
            Err(e) => (false, e, None),
        };
        println!("[{}] {name} ({ms:.0} ms){}", if ok { "PASS" } else { "FAIL" }, if detail.is_empty() { String::new() } else { format!(": {detail}") });
        self.steps.push((name.to_string(), ok, detail, ms));
        v
    }
}

fn ops(v: serde_json::Value) -> Result<Vec<Op>, String> {
    v.as_array().ok_or("ops must be an array")?.iter().map(|o| Op::from_json(o).map_err(|e| e.to_string())).collect()
}

fn ffmpeg(ffmpeg: &Path, args: &[&str]) -> Result<(), String> {
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-hide_banner", "-loglevel", "error", "-y"]).args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let out = cmd.output().map_err(|e| format!("ffmpeg: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!("ffmpeg {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim()))
    }
}

pub fn run(args: &[String]) -> i32 {
    let keep = args.iter().any(|a| a == "--keep");
    let dir = args.iter().position(|a| a == "--dir").and_then(|i| args.get(i + 1)).map(PathBuf::from).unwrap_or_else(|| std::env::temp_dir().join(format!("aiframes-selftest-{}", std::process::id())));
    let _ = std::fs::create_dir_all(&dir);
    println!("AIFrames {} self-test in {}", env!("CARGO_PKG_VERSION"), dir.display());
    let mut rep = Report { steps: Vec::new() };
    let started = Instant::now();

    // 1. Media.
    let media = rep.step("generate media with ffmpeg (lavfi)", || {
        let tools = aiframes_media::tools().map_err(|e| e.to_string())?;
        let video = dir.join("testsrc.mp4");
        let tone = dir.join("tone.wav");
        let still = dir.join("still.png");
        ffmpeg(&tools.ffmpeg, &["-f", "lavfi", "-i", "testsrc2=size=1280x720:rate=30", "-f", "lavfi", "-i", "sine=frequency=330:sample_rate=48000", "-t", "6", "-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac", "-shortest", &video.to_string_lossy()])?;
        ffmpeg(&tools.ffmpeg, &["-f", "lavfi", "-i", "sine=frequency=550:sample_rate=48000", "-t", "5", &tone.to_string_lossy()])?;
        ffmpeg(&tools.ffmpeg, &["-f", "lavfi", "-i", "color=c=0x8b6cff:size=640x360,drawbox=x=80:y=80:w=480:h=200:color=white@0.8:t=fill", "-frames:v", "1", &still.to_string_lossy()])?;
        Ok(((video, tone, still), format!("ffmpeg {}", tools.ffmpeg.display())))
    });

    // 2. Project.
    let services = Services::new();
    let project_path = dir.join("selftest.aifr");
    let built = media.as_ref().and_then(|(video, tone, still)| {
        rep.step("build and save a project", || {
            let mut doc = Document::new(crate::dialogs::new_project(1280, 720, 30.0));
            doc.save_as(&project_path).map_err(|e| e.to_string())?;
            let imp = actions::import_ops(&doc, &services.pool, &[video.clone(), tone.clone(), still.clone()])?;
            if imp.len() != 3 {
                return Err(format!("import_ops returned {} ops for 3 files", imp.len()));
            }
            doc.apply("user", "Import", imp).map_err(|e| e.to_string())?;
            let ids: Vec<String> = doc.project.media.iter().map(|m| m.id.clone()).collect();
            let (mv, mt, ms) = (&ids[0], &ids[1], &ids[2]);
            doc.apply(
                "user",
                "Rough cut",
                ops(json!([
                    {"op": "append_clip", "track": "v1", "clip": {"asset": mv, "duration": 4.0}},
                    {"op": "append_clip", "track": "v1", "clip": {"asset": ms, "duration": 2.0}},
                    {"op": "add_clip", "clip": {"track": "v2", "text": "AIFrames self-test", "start": 0.5, "duration": 2.5}},
                    {"op": "add_clip", "clip": {"track": "a2", "asset": mt, "start": 0.0, "duration": 5.0}},
                ]))?,
            )
            .map_err(|e| e.to_string())?;
            doc.save().map_err(|e| e.to_string())?;
            Ok(((), format!("{} clips, {:.2} s", doc.project.clips.len(), doc.resolved().duration)))
        })
    });

    // 3. Reopen.
    let doc_id = built.and_then(|_| {
        rep.step("reopen and validate", || {
            let doc = Document::open(&project_path).map_err(|e| e.to_string())?;
            let errors: Vec<String> = doc.validate().into_iter().filter(|f| f.severity == aiframes_core::Severity::Error).map(|f| format!("{}: {}", f.code, f.message)).collect();
            if !errors.is_empty() {
                return Err(errors.join("; "));
            }
            let n = doc.project.clips.len();
            let id = services.hub.add(ProjectDoc::new(doc));
            Ok((id, format!("{n} clips, no errors")))
        })
    });

    // 4. Playback path.
    if let Some(id) = doc_id {
        let snap = {
            let d = services.hub.get(id).expect("doc");
            let g = lock(&d);
            Arc::new(Snapshot { doc: id, revision: g.doc.revision(), epoch: 0, project: g.doc.project.clone(), resolved: g.doc.resolved().clone(), dir: g.doc.project_dir() })
        };
        let ctx = eframe::egui::Context::default();
        let mut engine = Engine::new(ctx, services.renderer.clone(), 3, snap.clone());
        engine.set_scale(0.5);
        rep.step("render paused frames through the engine", || {
            let mut n = 0;
            for t in [0.0, 1.0, 4.5] {
                engine.seek(t, false);
                let deadline = Instant::now() + Duration::from_secs(20);
                loop {
                    engine.update();
                    let s = engine.shown();
                    if s.texture.is_some() && !s.draft && s.size[0] == 640 {
                        n += 1;
                        break;
                    }
                    if Instant::now() > deadline {
                        return Err(format!("no frame at {t}s after 20 s (render error: {:?})", engine.stats.error));
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            if let Some(e) = &engine.stats.error {
                return Err(format!("render error: {e}"));
            }
            Ok(((), format!("{n} frames at 640x360, {:.1} ms avg", engine.stats.render_ms)))
        });
        rep.step("play 2 s in real time", || {
            engine.reset_stats();
            engine.play(0.0, 1.0, None);
            let t0 = Instant::now();
            let mut shown = 0u32;
            let mut last = None;
            while t0.elapsed() < Duration::from_secs(2) {
                engine.update();
                let s = engine.shown();
                if s.texture.is_some() && last != Some(engine_frame(&mut engine)) {
                    shown += 1;
                    last = Some(engine_frame(&mut engine));
                }
                std::thread::sleep(Duration::from_millis(4));
            }
            let t = engine.time();
            engine.pause();
            let st = engine.stats.clone();
            let detail = format!(
                "playhead {:.2} s after 2 s, {} dropped, render {:.1} ms, audio {} ({})",
                t,
                st.dropped,
                st.render_ms,
                if st.audio { "master clock" } else { "off (wall clock)" },
                engine.audio.device
            );
            if t < 1.5 {
                return Err(format!("playback stalled: {detail}"));
            }
            let _ = shown;
            Ok(((), detail))
        });
        rep.step("audio mix", || {
            let mut buf = vec![0f32; 4800 * 2];
            services.renderer.mix(&snap.dir, &snap.project, &snap.resolved, 1.0, &mut buf).map_err(|e| e.to_string())?;
            let rms = (buf.iter().map(|s| s * s).sum::<f32>() / buf.len() as f32).sqrt();
            if rms < 1e-4 {
                return Err(format!("the mix at 1 s is silent (rms {rms:.6}) although a tone and a video with sound play there"));
            }
            Ok(((), format!("rms {rms:.3}")))
        });
        drop(engine);

        // 5. Ops.
        rep.step("apply, undo and redo ops", || {
            let d = services.hub.get(id).expect("doc");
            let mut g = lock(&d);
            let r0 = g.doc.revision();
            let first = g.doc.project.clips.iter().find(|c| c.track == "v1").map(|c| c.id.clone()).ok_or("no clip on v1")?;
            g.doc.apply("user", "Split", ops(json!([{"op": "split_clip", "clip": first, "at": 2.0}]))?).map_err(|e| e.to_string())?;
            let n = g.doc.project.clips.len();
            g.doc.apply("user", "Marker", ops(json!([{"op": "add_marker", "time": 1.0, "name": "selftest"}]))?).map_err(|e| e.to_string())?;
            g.doc.undo().ok_or("undo failed")?;
            g.doc.redo().ok_or("redo failed")?;
            if g.doc.project.markers.len() != 1 {
                return Err("marker missing after redo".into());
            }
            Ok(((), format!("revision {r0} -> {}, {n} clips after split", g.doc.revision())))
        });

        // 6. Export.
        rep.step("export 2 s (H.264)", || {
            let out = dir.join("selftest-export.mp4");
            let settings = aiframes_core::ExportSettings { codec: "h264".into(), quality: "standard".into(), hardware: true, loudness_lufs: Some(-14.0), range: Some([0.0, 2.0]), path: Some(out.to_string_lossy().to_string()), ..Default::default() };
            let job = actions::start_export(&services, id, settings, out.clone())?;
            let deadline = Instant::now() + Duration::from_secs(180);
            let info = loop {
                let j = services.jobs.get(job).ok_or("job vanished")?;
                if j.status != JobStatus::Running {
                    break j;
                }
                if Instant::now() > deadline {
                    services.jobs.cancel(job);
                    return Err("export did not finish in 180 s".into());
                }
                std::thread::sleep(Duration::from_millis(100));
            };
            if info.status != JobStatus::Done {
                return Err(format!("export {:?}: {}", info.status, info.message));
            }
            let (_, mi) = aiframes_media::probe(&out).map_err(|e| format!("probe output: {e}"))?;
            if (mi.duration - 2.0).abs() > 0.25 {
                return Err(format!("output is {:.2} s, expected 2 s", mi.duration));
            }
            Ok(((), format!("{} ({}x{}, {:.2} s, {})", out.display(), mi.width, mi.height, mi.duration, mi.codec)))
        });

        // 7. The Shorts preset from the export dialog: a 16:9 project rendered to 1080x1920 (cover).
        rep.step("export 1 s with the Shorts preset (1080x1920, cover)", || {
            let out = dir.join("selftest-shorts.mp4");
            let project_settings = {
                let d = services.hub.get(id).expect("doc");
                let g = lock(&d);
                g.doc.project.settings.clone()
            };
            let p = crate::export::find("shorts").ok_or("no shorts preset")?;
            let size = crate::export::preset_size(p, &project_settings);
            let settings = crate::export::settings_for(p, Some([0.0, 1.0]), None, true, &out, size, p.fit, &project_settings);
            let job = actions::start_export(&services, id, settings, out.clone())?;
            let info = services.jobs.wait(job, Duration::from_secs(180)).ok_or("export did not finish in 180 s")?;
            if info.status != JobStatus::Done {
                return Err(format!("export {:?}: {}", info.status, info.message));
            }
            let (_, mi) = aiframes_media::probe(&out).map_err(|e| format!("probe output: {e}"))?;
            if (mi.width, mi.height) != (1080, 1920) {
                return Err(format!("output is {}x{}, expected 1080x1920", mi.width, mi.height));
            }
            Ok(((), format!("{}x{}, {:.2} s", mi.width, mi.height, mi.duration)))
        });
    }

    let failed = rep.steps.iter().filter(|s| !s.1).count();
    println!("\n{} of {} steps passed in {:.1} s{}", rep.steps.len() - failed, rep.steps.len(), started.elapsed().as_secs_f64(), if failed == 0 { "" } else { " — FAILED" });
    if !keep && failed == 0 {
        let _ = std::fs::remove_dir_all(&dir);
    } else {
        println!("files kept in {}", dir.display());
    }
    if failed == 0 {
        0
    } else {
        1
    }
}

/// The frame index currently shown (via the playhead), for counting distinct frames.
fn engine_frame(e: &mut Engine) -> i64 {
    let fps = e.snapshot().fps();
    crate::timecode::frame_index(e.time(), fps)
}
