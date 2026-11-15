//! Ported from the independent review (round 3). Defaults are small for debug test runs; use
//! `FUZZ_SEEDS=64 FUZZ_STEPS=1000 cargo test --release --test review_fuzz` for a long run (keep runs
//! with more than 300 successful edits per seed in mind: undo keeps the last 300 steps).
//! Random op sequences: invariants after every successful op, JSON round-trips, undo-all.
mod common;
use aiframes_core::*;
use common::*;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn f(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn range(&mut self, a: f64, b: f64) -> f64 {
        a + (b - a) * self.f()
    }
    fn n(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
    fn b(&mut self) -> bool {
        self.next() & 1 == 1
    }
    fn pick<'a, T>(&mut self, v: &'a [T]) -> Option<&'a T> {
        if v.is_empty() {
            None
        } else {
            Some(&v[self.n(v.len())])
        }
    }
}

fn r2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

fn start_project() -> Project {
    let mut p = base();
    // FUZZ_FPS=29.97 (etc.) fuzzes the frame snapping on another grid.
    if let Some(fps) = std::env::var("FUZZ_FPS").ok().and_then(|s| s.parse().ok()) {
        p.settings.fps = fps;
    }
    ap(&mut p, json!({"op":"add_clip","clip":{"id":"c1","track":"v1","asset":"mv","start":0.0,"duration":5.0}}));
    ap(&mut p, json!({"op":"append_clip","track":"v1","clip":{"id":"c2","asset":"mv","in_point":10.0,"duration":6.0}}));
    ap(&mut p, json!({"op":"append_clip","track":"v1","clip":{"id":"c3","asset":"mv","in_point":30.0,"duration":4.0,"speed":2.0}}));
    ap(&mut p, json!({"op":"add_clip","clip":{"id":"vo","track":"a1","asset":"m1","start":1.0,"duration":12.0,"in_point":0.5}}));
    ap(&mut p, json!({"op":"add_clip","clip":{"id":"t1","track":"v2","text":"title","start":{"with":"c2","offset":0.5},"duration":2.0,"fade_in":0.5,"fade_out":0.5,
        "opacity":{"keys":[{"t":0,"v":0},{"t":1,"v":1,"ease":"out"}]}}}));
    ap(&mut p, json!({"op":"add_clip","clip":{"id":"t2","track":"v2","text":"word","start":{"word":{"clip":"vo","index":8}},"duration":1.0}}));
    ap(&mut p, json!({"op":"add_marker","id":"mk1","time":{"after":"c1","offset":-0.5},"name":"m"}));
    ap(&mut p, json!({"op":"add_transition","from":"c1","to":"c2","kind":"crossfade","duration":0.5}));
    p
}

fn json_close(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => {
            let (x, y) = (x.as_f64().unwrap(), y.as_f64().unwrap());
            (x - y).abs() <= 1e-12 * x.abs().max(1.0)
        }
        (Value::Array(x), Value::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(p, q)| json_close(p, q)),
        (Value::Object(x), Value::Object(y)) => x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| json_close(v, w))),
        _ => a == b,
    }
}

fn clip_ids(p: &Project) -> Vec<String> {
    p.clips.iter().map(|c| c.id.clone()).collect()
}

fn gen_op(rng: &mut Rng, p: &Project) -> Option<Value> {
    let r = resolve(p);
    let ids = clip_ids(p);
    let id = rng.pick(&ids)?.clone();
    let sp = r.span(&id);
    let dur = r.duration.max(1.0);
    let t_in = |rng: &mut Rng| sp.map(|s| r2(rng.range(s.start, s.end))).unwrap_or(1.0);
    let other = rng.pick(&ids)?.clone();
    let tracks = ["v1", "v2", "a1"];
    let track = tracks[rng.n(3)];
    let media_ids: Vec<String> = p.clips.iter().filter(|c| matches!(c.source, Source::Media { .. })).map(|c| c.id.clone()).collect();
    let k = rng.n(34);
    Some(match k {
        0 => json!({"op":"move_clip","clip":id,"start":r2(rng.range(0.0, dur))}),
        1 => {
            let a = ["after", "with", "before"][rng.n(3)];
            json!({"op":"move_clip","clip":id,"start":{a: other, "offset": r2(rng.range(-0.5, 0.5))}})
        }
        2 => json!({"op":"trim_clip","clip":id,"edge":"start","delta":r2(rng.range(-1.0, 1.5)),"ripple": ([Value::Null, json!(true), json!(false)][rng.n(3)])}),
        3 => json!({"op":"trim_clip","clip":id,"edge":"end","delta":r2(rng.range(-1.5, 1.0)),"ripple": ([Value::Null, json!(true), json!(false)][rng.n(3)])}),
        4 => json!({"op":"split_clip","clip":id,"at":t_in(rng)}),
        5 => json!({"op":"delete_clip","clip":id,"ripple":rng.b()}),
        6 => json!({"op":"duplicate_clip","clip":id,"start":r2(rng.range(0.0, dur + 3.0)),"track":track}),
        7 => json!({"op":"slip_clip","clip":id,"delta":r2(rng.range(-1.0, 1.0))}),
        8 => json!({"op":"roll_edit","left":id,"right":other,"delta":r2(rng.range(-1.0, 1.0))}),
        9 => json!({"op":"slide_clip","clip":id,"delta":r2(rng.range(-1.0, 1.0))}),
        10 => json!({"op":"set_speed","clip":id,"speed":([0.5, 1.0, 1.5, 2.0][rng.n(4)]),"keep":(["source","duration"][rng.n(2)])}),
        11 => json!({"op":"freeze_frame","clip":id,"at":t_in(rng),"duration":r2(rng.range(0.2, 1.5))}),
        12 => json!({"op":"detach_audio","clip":id,"track":"a1"}),
        13 => {
            let a = r2(rng.range(0.0, dur));
            json!({"op":"lift_range","from":a,"to":r2(a + rng.range(0.1, 3.0))})
        }
        14 => {
            let a = r2(rng.range(0.0, dur));
            json!({"op":"extract_range","from":a,"to":r2(a + rng.range(0.1, 3.0))})
        }
        15 => json!({"op":"nudge","clips":[id],"delta":r2(rng.range(-1.0, 1.0))}),
        16 => json!({"op":"insert_clip","track":(["v1","v2"][rng.n(2)]),"at":r2(rng.range(0.0, dur)),"clip":{"asset":"mv","in_point":r2(rng.range(0.0, 50.0)),"duration":r2(rng.range(0.5, 3.0))}}),
        17 => json!({"op":"append_clip","track":(["v1","v2"][rng.n(2)]),"clip":{"text":"app","duration":r2(rng.range(0.5, 3.0))}}),
        18 => {
            let m = rng.pick(&media_ids)?.clone();
            let a = rng.n(40);
            json!({"op":"delete_words","clip":m,"from":a,"to":a + rng.n(3)})
        }
        19 => {
            let m = rng.pick(&media_ids)?.clone();
            let a = r2(rng.range(0.0, 40.0));
            json!({"op":"cut_source","clip":m,"from":a,"to":r2(a + rng.range(0.1, 2.0))})
        }
        20 => {
            let m = rng.pick(&media_ids)?.clone();
            json!({"op":"generate_captions","clips":[m],"track":"v2","max_words":1 + rng.n(4)})
        }
        21 => json!({"op":"add_keyframe","clip":id,"path":(["opacity","transform.scale","volume","transform.rotation_x","transform.rotation_y"][rng.n(5)]),"t":r2(rng.range(0.0, p.clips.iter().find(|c| c.id == id)?.duration)),"v":r2(rng.range(0.0, 1.0)),"ease":(["linear","in","out","in_out","back_out","hold"][rng.n(6)])}),
        22 => json!({"op":"add_transition","from":id,"to":other,"kind":(["crossfade","spin","zoom_through","shake","glitch","flash_zoom","film_burn","cube","flip","slide_blur","stretch","luma_wipe"][rng.n(12)]),"duration":0.3}),
        23 => json!({"op":"set_param","clip":id,"path":"speed","keys":[{"t":0,"v":1,"ease":(["linear","in","out","in_out","back_out"][rng.n(5)])},{"t":r2(rng.range(0.5,3.0)),"v":r2(rng.range(0.5,2.5))}]}),
        24 => json!({"op":"add_marker","time":{(["after","with","before"][rng.n(3)]): id, "offset": r2(rng.range(-0.5,0.5))}}),
        25 => json!({"op":"move_clips","moves":[{"clip":id,"start":r2(rng.range(0.0,dur))},{"clip":other,"start":{"after":id}}]}),
        26 => match rng.n(4) {
            // Round 6: track mattes, motion blur, frame blending, 3-D cards.
            0 => json!({"op":"set_clip","clip":id,"patch":{"matte":{"clip":other,"mode":(["alpha","alpha_inverted","luma","luma_inverted"][rng.n(4)])}}}),
            1 => json!({"op":"set_clip","clip":id,"patch":{"matte":null,"motion_blur":{"shutter":r2(rng.range(0.0, 720.0)),"samples":2 + rng.n(30)}}}),
            2 => json!({"op":"set_clip","clip":id,"patch":{"frame_blend":rng.b(),"transform":{"perspective":r2(rng.range(0.0, 4000.0))}}}),
            _ => json!({"op":"add_effect","clip":id,"effect":(["directional_blur","glitch","echo","stutter","edge_glow","lens_distortion","exposure_flash"][rng.n(7)])}),
        },
        27 => json!({"op":"remove_track","track":track,"force":true}),
        28 => json!({"op":"set_transcript","asset":"m1","transcript":null,"force":true}),
        29 => json!({"op":"remove_marker","marker":"mk1"}),
        30 => json!({"op":"add_effect","clip":id,"effect":"blur"}),
        31 => json!({"op":"remove_media","asset":"m1","force":true}),
        32 => json!({"op":"duplicate_clip","clip":id}),
        _ => json!({"op":"set_clip","clip":id,"patch":{"fade_in":r2(rng.range(0.0,1.0)),"reverse":rng.b()}}),
    })
}

fn counts(p: &Project) -> BTreeMap<&'static str, HashSet<String>> {
    let r = resolve(p);
    let mut m: BTreeMap<&'static str, HashSet<String>> = BTreeMap::new();
    for f in validate(p, &r) {
        if matches!(
            f.code,
            "overlap_same_track" | "clip_past_media_end" | "clip_before_media_start" | "negative_start" | "transition_not_adjacent" | "unresolved_anchor" | "anchor_cycle" | "word_cut_out" | "keys_unsorted" | "zero_length_clip" | "track_kind_mismatch" | "duplicate_id" | "matte_unknown" | "matte_not_visual" | "matte_cycle"
        ) && f.severity == Severity::Error
        {
            m.entry(f.code).or_default().insert(f.ids.join(","));
        }
    }
    m
}

#[test]
fn fuzz_ops_invariants_roundtrip_undo() {
    let seeds: u64 = std::env::var("FUZZ_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(40);
    let steps: usize = std::env::var("FUZZ_STEPS").ok().and_then(|s| s.parse().ok()).unwrap_or(40);
    let mut violations: BTreeMap<String, (usize, String)> = BTreeMap::new();
    let mut note = |kind: String, detail: String| {
        let e = violations.entry(kind).or_insert((0, detail));
        e.0 += 1;
    };
    let mut applied_total = 0;
    for seed in 1..=seeds {
        let mut rng = Rng(seed.wrapping_mul(0x9E3779B97F4A7C15) | 1);
        let p0 = start_project();
        let mut doc = Document::new(p0.clone());
        let mut trail: Vec<Value> = Vec::new();
        let mut applied = 0;
        for _ in 0..steps {
            let Some(v) = gen_op(&mut rng, &doc.project) else { continue };
            let o = match Op::from_json(&v) {
                Ok(o) => o,
                Err(e) => {
                    note(format!("op does not parse: {}", v["op"]), format!("{v}: {e}"));
                    continue;
                }
            };
            // Op JSON round trip.
            let s = serde_json::to_string(&o).unwrap();
            match serde_json::from_str::<Op>(&s) {
                Ok(o2) if o2 == o => {}
                other => note(format!("op round trip {}", o.name()), format!("{s} -> {other:?}")),
            }
            let before = doc.project.clone();
            let cb = counts(&before);
            let rb = resolve(&before);
            let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| doc.apply("fuzz", "", vec![o.clone()])));
            let res = match res {
                Ok(r) => r,
                Err(_) => {
                    note(format!("PANIC in {}", o.name()), format!("seed {seed} trail {trail:?} then {v}"));
                    break;
                }
            };
            let Ok(receipt) = res else { continue };
            applied += 1;
            trail.push(v.clone());
            let after = &doc.project;
            let ra = resolve(after);
            // 1. no new resolve errors
            let old: HashSet<&str> = rb.errors.iter().map(|e| e.id.as_str()).collect();
            for e in ra.errors.iter().filter(|e| !old.contains(e.id.as_str())) {
                note(format!("new resolve error after {}", o.name()), format!("seed {seed}: {} :: trail {trail:?}", e.message));
            }
            // 2. new validate errors of the guarded kinds
            let ca = counts(after);
            for (code, set) in &ca {
                let prev = cb.get(code).cloned().unwrap_or_default();
                for ids in set.difference(&prev) {
                    note(format!("new {code} after {}", o.name()), format!("seed {seed}: ids {ids} :: last {v} :: trail len {}", trail.len()));
                }
            }
            // 3. project JSON round trip
            let text = aiframes_core::document::canonical_json(after);
            match aiframes_core::document::parse_project(&text) {
                Ok(q) if &q == after => {}
                Ok(q) if json_close(&serde_json::to_value(&q).unwrap(), &serde_json::to_value(after).unwrap()) => {
                    note("f64 values drift by 1 ulp through canonical_json/parse_project".into(), format!("seed {seed}"));
                }
                Ok(q) => { let a = format!("{after:#?}"); let b = format!("{q:#?}"); let d = a.lines().zip(b.lines()).enumerate().find(|(_, (x, y))| x != y).map(|(i,(x,y))| format!("line {i}: {x} || {y}")); note(format!("project round trip differs after {}", o.name()), format!("seed {seed}: {d:?}")) }
                Err(e) => note(format!("project does not re-parse after {}", o.name()), format!("seed {seed}: {e}")),
            }
            // 4. receipt: every clip whose span changed is listed
            let moved: HashSet<&str> = receipt.moved.iter().map(|m| m.id.as_str()).collect();
            for c in &after.clips {
                let (x, y) = (rb.span(&c.id), ra.span(&c.id));
                let changed = match (x, y) {
                    (Some(a), Some(b)) => (a.start - b.start).abs() > 1e-9 || (a.end - b.end).abs() > 1e-9,
                    (None, None) => false,
                    _ => true,
                };
                if changed && !moved.contains(c.id.as_str()) {
                    note("receipt misses a moved clip".into(), format!("seed {seed} {}", c.id));
                }
            }
            // 5. dangling references
            let cids: HashSet<&str> = after.clips.iter().map(|c| c.id.as_str()).collect();
            for t in &after.transitions {
                if !cids.contains(t.from.as_str()) || !cids.contains(t.to.as_str()) {
                    note(format!("dangling transition after {}", o.name()), format!("seed {seed}"));
                }
            }
            let tids: HashSet<&str> = after.tracks.iter().map(|t| t.id.as_str()).collect();
            for c in &after.clips {
                if !tids.contains(c.track.as_str()) {
                    note(format!("clip on missing track after {}", o.name()), format!("seed {seed}"));
                }
            }
            // 6. frame snapping never changes an audio clip's length or opens/closes gaps between sound
            for b in audio_snap_violations(after) {
                note(format!("audio snapping after {}", o.name()), format!("seed {seed}: {b} :: last {v}"));
            }
        }
        applied_total += applied;
        // Undo everything -> original.
        let mut n = 0;
        while doc.undo().is_some() {
            n += 1;
        }
        if n != applied {
            note("undo count".into(), format!("seed {seed}: {n} undos for {applied} edits"));
        }
        if doc.project != p0 {
            note("undo-all differs from original".into(), format!("seed {seed}"));
        }
        // Redo everything -> replay equals redo.
        let mut replay = p0.clone();
        for v in &trail {
            if apply(&mut replay, &op(v.clone())).is_err() {
                note("replay of the trail fails".into(), format!("seed {seed}: {v}"));
                break;
            }
        }
        while doc.redo().is_some() {}
        if doc.project != replay {
            note("redo-all differs from replaying ops".into(), format!("seed {seed}"));
        }
    }
    println!("applied {applied_total} ops");
    for (k, (n, ex)) in &violations {
        println!("[{n:>4}] {k}\n        e.g. {}", ex.chars().take(600).collect::<String>());
    }
    assert!(violations.is_empty(), "{} violation kinds", violations.len());
}

fn error_keys(p: &Project) -> HashSet<String> {
    let r = resolve(p);
    validate(p, &r).into_iter().filter(|f| f.severity == Severity::Error).map(|f| format!("{}:{}", f.code, f.ids.join(","))).collect()
}

/// Random states from the op fuzzer, plus hand-made breakage; every fix_ops must apply, remove its
/// finding, and add no new error.
#[test]
fn fuzz_fix_ops() {
    let seeds: u64 = std::env::var("FUZZ_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(60);
    let mut bad: BTreeMap<String, (usize, String)> = BTreeMap::new();
    let mut tried: BTreeMap<&'static str, usize> = BTreeMap::new();
    for seed in 1..=seeds {
        let mut rng = Rng(seed.wrapping_mul(0x9E3779B97F4A7C15) | 1);
        let mut p = start_project();
        for _ in 0..30 {
            if let Some(v) = gen_op(&mut rng, &p) {
                if let Ok(o) = Op::from_json(&v) {
                    let _ = apply(&mut p, &o);
                }
            }
        }
        // hand-made breakage (as from a hand-edited file or relink)
        let n = p.clips.len();
        if n > 0 {
            let i = rng.n(n);
            match rng.n(6) {
                0 => p.media[0].info.duration = 8.0, // clips past media end
                1 => p.clips[i].fade_in = p.clips[i].duration, // fades too long
                2 => p.settings.duration = Some(6.0),
                3 => {
                    if let Some(j) = (0..n).find(|&j| j != i && p.clips[j].track == p.clips[i].track) {
                        let s = resolve(&p).span(&p.clips[j].id).map(|s| s.start + 0.2).unwrap_or(0.0);
                        p.clips[i].start = Time::At(s); // overlap
                    }
                }
                4 => p.clips[i].start = Time::At(-1.0),
                _ => {
                    if let Param::Value(_) = p.clips[i].opacity {
                        p.clips[i].opacity = Param::Animated { keys: vec![Key { t: -1.0, v: 0.0, ease: Ease::Linear }, Key { t: p.clips[i].duration + 2.0, v: 1.0, ease: Ease::Linear }] };
                    }
                }
            }
        }
        let r = resolve(&p);
        let before_errors = error_keys(&p);
        for f in validate(&p, &r) {
            if f.fix_ops.is_empty() {
                continue;
            }
            *tried.entry(f.code).or_default() += 1;
            let mut q = p.clone();
            match apply_batch(&mut q, &f.fix_ops) {
                Err(e) => {
                    let e2 = bad.entry(format!("fix_ops of {} fail", f.code)).or_insert((0, format!("seed {seed}: {} :: {} :: {e}", f.message, serde_json::to_string(&f.fix_ops).unwrap())));
                    e2.0 += 1;
                }
                Ok(_) => {
                    let rq = resolve(&q);
                    let left = validate(&q, &rq).into_iter().any(|g| g.code == f.code && g.ids == f.ids);
                    if left {
                        let e2 = bad.entry(format!("fix_ops of {} do not remove it", f.code)).or_insert((0, format!("seed {seed}: {} :: {}", f.message, serde_json::to_string(&f.fix_ops).unwrap())));
                        e2.0 += 1;
                    }
                    let after = error_keys(&q);
                    for k in after.difference(&before_errors) {
                        let e2 = bad.entry(format!("fix_ops of {} add error {}", f.code, k.split(':').next().unwrap())).or_insert((0, format!("seed {seed}: {} :: {} -> {k}", f.message, serde_json::to_string(&f.fix_ops).unwrap())));
                        e2.0 += 1;
                    }
                }
            }
        }
    }
    println!("tried: {tried:?}");
    for (k, (n, ex)) in &bad {
        println!("[{n:>4}] {k}\n        e.g. {}", ex.chars().take(700).collect::<String>());
    }
    assert!(bad.is_empty());
}

/// Picture with linked (detached) sound placed off the frame grid, an L-cut (sound longer than its
/// picture, not linked) and a voice chain, on several frame rates: after every successful op the audio
/// side of frame snapping holds (lengths kept, touching pieces still touch, nothing pushed into overlap).
#[test]
fn fuzz_audio_follows_frame_snapping() {
    let seeds: u64 = std::env::var("FUZZ_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(40);
    let steps: usize = std::env::var("FUZZ_STEPS").ok().and_then(|s| s.parse().ok()).unwrap_or(40);
    let mut violations: BTreeMap<String, (usize, String)> = BTreeMap::new();
    let mut applied = 0;
    for seed in 1..=seeds {
        let fps = [30.0, 29.97, 25.0, 23.976, 60.0][(seed % 5) as usize];
        let mut p = base();
        p.settings.fps = fps;
        ap(&mut p, json!({"op":"add_clip","clip":{"id":"c1","track":"v1","asset":"mv","start":0.013,"duration":6.0}}));
        ap(&mut p, json!({"op":"detach_audio","clip":"c1","track":"a1","new_id":"s1"}));
        ap(&mut p, json!({"op":"add_clip","clip":{"id":"c2","track":"v1","asset":"mv","in_point":20.0,"start":{"after":"c1","offset":0.27},"duration":4.0}}));
        ap(&mut p, json!({"op":"add_clip","clip":{"id":"c3","track":"a1","asset":"mv","in_point":20.0,"start":{"with":"c2"},"duration":5.0}}));
        ap(&mut p, json!({"op":"add_clip","clip":{"id":"t1","track":"v2","text":"t","start":{"word":{"clip":"c3","index":22}},"duration":1.0}}));
        let mut rng = Rng(seed.wrapping_mul(0xD1B54A32D192ED03) | 1);
        let mut trail: Vec<Value> = Vec::new();
        for _ in 0..steps {
            let Some(v) = gen_op(&mut rng, &p) else { continue };
            let Ok(o) = Op::from_json(&v) else { continue };
            let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut q = p.clone();
                apply(&mut q, &o).map(|_| q)
            }));
            let Ok(res) = res else {
                violations.entry(format!("PANIC in {}", o.name())).or_insert((0, format!("seed {seed} fps {fps}: {v}"))).0 += 1;
                break;
            };
            let Ok(q) = res else { continue };
            p = q;
            applied += 1;
            trail.push(v.clone());
            for b in audio_snap_violations(&p) {
                violations.entry(format!("audio snapping after {}", o.name())).or_insert((0, format!("seed {seed} fps {fps}: {b} :: trail {trail:?}"))).0 += 1;
            }
        }
    }
    println!("applied {applied} ops");
    for (k, (n, ex)) in &violations {
        println!("[{n:>4}] {k}\n        e.g. {}", ex.chars().take(900).collect::<String>());
    }
    assert!(violations.is_empty(), "{} violation kinds", violations.len());
}

/// Clips on `track` that touch (shown spans), in time order.
fn touching_pairs(p: &Project, track: &str) -> Vec<(Id, Id)> {
    let r = resolve(p);
    let mut on: Vec<(Id, Span)> = p.clips.iter().filter(|c| c.track == track).filter_map(|c| Some((c.id.clone(), r.span(&c.id)?))).collect();
    on.sort_by(|a, b| a.1.start.total_cmp(&b.1.start));
    on.windows(2).filter(|w| (w[0].1.end - w[1].1.start).abs() < 1e-6).map(|w| (w[0].0.clone(), w[1].0.clone())).collect()
}

/// Ops that promise to keep cuts closed (roll, slide, ripple trims, freezes) never open a gap or make
/// an overlap on a track whose clips end with `until` anchors (words, clips, absolute times), off the
/// frame grid, on several frame rates.
#[test]
fn fuzz_cuts_stay_closed() {
    let seeds: u64 = std::env::var("FUZZ_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(40);
    let steps: usize = std::env::var("FUZZ_STEPS").ok().and_then(|s| s.parse().ok()).unwrap_or(40);
    let mut violations: BTreeMap<String, (usize, String)> = BTreeMap::new();
    let mut applied = 0;
    // FUZZ_SEED=n runs one seed (with FUZZ_FULL=1 the whole trail is printed).
    let only: Option<u64> = std::env::var("FUZZ_SEED").ok().and_then(|s| s.parse().ok());
    for seed in 1..=seeds {
        if only.is_some_and(|o| o != seed) {
            continue;
        }
        let fps = [30.0, 29.97, 25.0, 23.976, 60.0][(seed % 5) as usize];
        let mut p = base();
        p.settings.fps = fps;
        apply_batch(
            &mut p,
            &[
                op(json!({"op":"add_clip","clip":{"id":"vo","track":"a1","asset":"m1","start":0.37,"duration":30.0}})),
                op(json!({"op":"add_clip","clip":{"id":"k1","track":"v1","asset":"mv","start":0.013,"duration":3.0,"until":{"word":{"clip":"vo","index":5},"offset":-0.04}}})),
                op(json!({"op":"add_clip","clip":{"id":"k2","track":"v1","asset":"mv","in_point":20.0,"start":{"word":{"clip":"vo","index":5},"offset":-0.04},"duration":3.0,"until":{"word":{"clip":"vo","index":9}}}})),
                op(json!({"op":"add_clip","clip":{"id":"k3","track":"v1","asset":"mv","in_point":40.0,"start":{"after":"k2"},"duration":3.0,"until":14.37}})),
                op(json!({"op":"add_clip","clip":{"id":"k4","track":"v1","asset":"mv","in_point":60.0,"start":{"after":"k3"},"duration":2.51}})),
                op(json!({"op":"add_clip","clip":{"id":"k5","track":"v1","asset":"mv","in_point":70.0,"start":{"after":"k4"},"duration":2.0,"until":{"after":"vo","offset":-3.3}}})),
                op(json!({"op":"add_clip","clip":{"id":"b1","track":"v2","text":"badge","start":{"with":"k3","offset":0.21},"duration":1.0,"until":{"after":"k3"}}})),
            ],
        )
        .unwrap_or_else(|e| panic!("start project: {e}"));
        let mut rng = Rng(seed.wrapping_mul(0xA24BAED4963EE407) | 1);
        let mut trail: Vec<Value> = Vec::new();
        for _ in 0..steps {
            let r = resolve(&p);
            let v1: Vec<Id> = p.clips.iter().filter(|c| c.track == "v1").map(|c| c.id.clone()).collect();
            let pairs = touching_pairs(&p, "v1");
            let d = r2(rng.range(-1.2, 1.2));
            let v = match rng.n(6) {
                0 | 1 => match rng.pick(&pairs) {
                    Some((a, b)) => json!({"op":"roll_edit","left":a,"right":b,"delta":d}),
                    None => continue,
                },
                2 => json!({"op":"slide_clip","clip":rng.pick(&v1).cloned().unwrap_or_default(),"delta":d}),
                3 => json!({"op":"trim_clip","clip":rng.pick(&v1).cloned().unwrap_or_default(),"edge":(["start","end"][rng.n(2)]),"delta":d,"ripple":true}),
                4 => {
                    let Some(id) = rng.pick(&v1).cloned() else { continue };
                    let Some(s) = r.span(&id) else { continue };
                    json!({"op":"freeze_frame","clip":id,"at":r2(rng.range(s.start, s.end)),"duration":r2(rng.range(0.2, 1.5))})
                }
                _ => match gen_op(&mut rng, &p) {
                    Some(v) => v,
                    None => continue,
                },
            };
            let Ok(o) = Op::from_json(&v) else { continue };
            let before = p.clone();
            let track_pairs = |p: &Project, id: &str| -> Vec<(Id, Id)> { p.clips.iter().find(|c| c.id == id).map(|c| touching_pairs(p, &c.track)).unwrap_or_default() };
            let target = v["clip"].as_str().or(v["left"].as_str()).unwrap_or("").to_string();
            // Captions are re-timed to their words after every op (sliding one is undone on purpose).
            let caption = before.clips.iter().any(|c| c.id == target && c.tags.iter().any(|t| t == "caption"));
            let pairs_before = if caption { Vec::new() } else { track_pairs(&before, &target) };
            let len_before = resolve(&before).span(&target).map(|s| s.duration());
            let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut q = p.clone();
                apply(&mut q, &o).map(|_| q)
            }));
            let Ok(res) = res else {
                violations.entry(format!("PANIC in {}", o.name())).or_insert((0, format!("seed {seed}: {v}"))).0 += 1;
                break;
            };
            let Ok(q) = res else { continue };
            p = q;
            applied += 1;
            trail.push(v.clone());
            let ra = resolve(&p);
            let mut bad = |what: String| {
                violations.entry(format!("{} {what}", o.name())).or_insert((0, format!("seed {seed} fps {fps}: {v} :: trail {}", serde_json::to_string(&trail).unwrap_or_default()))).0 += 1;
            };
            let touch = |a: &str, b: &str| match (ra.span(a), ra.span(b)) {
                (Some(x), Some(y)) => (x.end - y.start).abs() < 1e-6,
                _ => true,
            };
            match o.name() {
                "roll_edit" => {
                    if !touch(v["left"].as_str().unwrap(), v["right"].as_str().unwrap()) {
                        bad("opened its cut".into());
                    }
                }
                "slide_clip" | "freeze_frame" => {
                    // A freeze splits its clip: cuts of that clip itself change on purpose.
                    for (a, b) in pairs_before.iter().filter(|(a, b)| o.name() == "slide_clip" || (a != &target && b != &target)) {
                        if !touch(a, b) {
                            bad(format!("opened the cut {a}|{b}"));
                        }
                    }
                    if o.name() == "slide_clip" && !caption {
                        if let (Some(l0), Some(l1)) = (len_before, ra.span(&target).map(|s| s.duration())) {
                            if (l0 - l1).abs() > 1e-6 {
                                bad(format!("changed the slid clip's length {l0} -> {l1}"));
                            }
                        }
                    }
                }
                "trim_clip" if v["ripple"] == json!(true) => {
                    for (a, b) in &pairs_before {
                        if !touch(a, b) {
                            bad(format!("opened the cut {a}|{b}"));
                        }
                    }
                }
                _ => {}
            }
            for b in audio_snap_violations(&p) {
                violations.entry(format!("audio snapping after {}", o.name())).or_insert((0, format!("seed {seed}: {b}"))).0 += 1;
            }
        }
    }
    println!("applied {applied} ops");
    for (k, (n, ex)) in &violations {
        println!("[{n:>4}] {k}\n        e.g. {}", ex.chars().take(if std::env::var("FUZZ_FULL").is_ok() { usize::MAX } else { 1500 }).collect::<String>());
    }
    assert!(violations.is_empty(), "{} violation kinds", violations.len());
}
