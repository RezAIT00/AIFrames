//! "Follow AI": bringing what the AI just changed into view without getting in the person's way,
//! and the recap of what an AI session changed. The timeline itself does the scrolling
//! (`Timeline::reveal`: smallest change, rows too); this module decides what to show.

use aiframes_core::{Id, Project, Resolved};
use std::collections::HashSet;

/// Tags (or a "temporary" note) that mark AI scaffolding the view should not chase.
pub fn is_temporary(tags: &[String], notes: &str) -> bool {
    tags.iter().any(|t| matches!(t.to_ascii_lowercase().as_str(), "temp" | "temporary" | "probe" | "scratch" | "ai-temp"))
        || notes.to_ascii_lowercase().starts_with("temporary")
}

/// One changed thing on the timeline: its time span and (for clips) its track.
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub t0: f64,
    pub t1: f64,
    pub track: Option<Id>,
}

/// What to bring into view: the changed items in time order.
#[derive(Clone, Debug, PartialEq)]
pub struct Target {
    pub t0: f64,
    pub t1: f64,
    /// Tracks of the clips, in display order (top row first; markers add none).
    pub tracks: Vec<Id>,
    pub items: Vec<Item>,
}

impl Target {
    /// True when the whole range is inside the visible `(start, end)` of the timeline.
    pub fn on_screen(&self, visible: (f64, f64)) -> bool {
        self.t0 >= visible.0 && self.t1 <= visible.1
    }

    pub fn span(&self) -> f64 {
        self.t1 - self.t0
    }

    /// The first item (earliest) cut to `width` seconds, so revealing it never needs a zoom out.
    pub fn first_within(&self, width: f64) -> Target {
        let first = &self.items[0];
        let t1 = first.t1.min(first.t0 + (width * 0.9).max(0.0));
        Target { t0: first.t0, t1, tracks: first.track.iter().cloned().collect(), items: vec![Item { t1, ..first.clone() }] }
    }
}

/// The time range and tracks covered by these ids (clips that still exist and are not temporary,
/// and markers).
pub fn target(p: &Project, r: &Resolved, ids: &[Id]) -> Option<Target> {
    let mut items: Vec<Item> = Vec::new();
    for id in ids {
        if let Some(c) = p.clips.iter().find(|c| &c.id == id) {
            if is_temporary(&c.tags, &c.notes) {
                continue;
            }
            if let Some(s) = r.span(id) {
                items.push(Item { t0: s.start, t1: s.end, track: Some(c.track.clone()) });
            }
        } else if let Some(t) = r.marker(id) {
            items.push(Item { t0: t, t1: t, track: None });
        }
    }
    if items.is_empty() {
        return None;
    }
    items.sort_by(|a, b| a.t0.total_cmp(&b.t0));
    let t0 = items.iter().map(|i| i.t0).fold(f64::INFINITY, f64::min);
    let t1 = items.iter().map(|i| i.t1).fold(f64::NEG_INFINITY, f64::max);
    // Display order (the timeline shows the topmost when they cannot all fit).
    let used: HashSet<&str> = items.iter().filter_map(|i| i.track.as_deref()).collect();
    let tracks = aiframes_core::tracks_in_display_order(p).into_iter().filter(|t| used.contains(t.id.as_str())).map(|t| t.id.clone()).collect();
    Some(Target { t0, t1, tracks, items })
}

/// How to follow an AI edit, given what the person sees and does.
#[derive(Clone, Debug, PartialEq)]
pub enum Follow {
    /// Show all of it (the timeline scrolls; it zooms out only when the edit is wider than the view).
    All(Target),
    /// Keep the person's zoom: scroll to the first changed item and offer the rest (chip).
    First(Target),
    /// Nothing to do on the timeline (already in view); only rows may need scrolling.
    InView(Target),
}

/// Decides how to follow an edit: never zoom out when the person set the view recently or the edit is
/// wider than the view (scroll to its first item instead), never zoom in.
pub fn plan(target: &Target, visible: (f64, f64), person_set_view_recently: bool) -> Follow {
    let width = (visible.1 - visible.0).max(0.0);
    if target.on_screen(visible) {
        Follow::InView(target.clone())
    } else if !person_set_view_recently && target.span() <= width {
        Follow::All(target.clone())
    } else {
        let first = target.first_within(width);
        if first.on_screen(visible) {
            Follow::InView(first)
        } else {
            Follow::First(first)
        }
    }
}

/// What an AI session changed between `before` and `after`, given the ids its edits reported
/// (`touched`: created and changed): `[added, changed, removed, temporary]`.
pub fn session_recap(before: &Project, after: &Project, touched: &[Id]) -> [Vec<Id>; 4] {
    let d = aiframes_core::ops::diff_projects(before, after);
    let ids = |p: &Project| -> HashSet<Id> {
        let mut s: HashSet<Id> = p.clips.iter().map(|c| c.id.clone()).collect();
        s.extend(p.markers.iter().map(|m| m.id.clone()));
        s.extend(p.transitions.iter().map(|t| t.id.clone()));
        s.extend(p.tracks.iter().map(|t| t.id.clone()));
        s.extend(p.media.iter().map(|m| m.id.clone()));
        s
    };
    let (was, now) = (ids(before), ids(after));
    let touched_set: HashSet<&Id> = touched.iter().collect();
    let added: Vec<Id> = d.created.iter().filter(|i| touched_set.contains(i)).cloned().collect();
    let changed: Vec<Id> = d.changed.iter().filter(|i| touched_set.contains(i)).cloned().collect();
    let removed: Vec<Id> = d.removed.clone();
    let mut temporary: Vec<Id> = Vec::new();
    for i in touched {
        if !was.contains(i) && !now.contains(i) && !temporary.contains(i) {
            temporary.push(i.clone());
        }
    }
    [added, changed, removed, temporary]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> Project {
        serde_json::from_value(serde_json::json!({
            "format": 1,
            "tracks": [{"id": "v1", "kind": "video"}, {"id": "v2", "kind": "video"}, {"id": "a1", "kind": "audio"}],
            "clips": [
                {"id": "a", "track": "v1", "start": 10.0, "duration": 2.0, "source": {"type": "solid", "color": "#000000"}},
                {"id": "probe", "track": "v1", "start": 40.0, "duration": 2.0, "source": {"type": "solid", "color": "#000000"}, "tags": ["probe"]},
                {"id": "b", "track": "v2", "start": 50.0, "duration": 5.0, "source": {"type": "solid", "color": "#000000"}}
            ],
            "markers": [{"id": "mk1", "time": 30.0}]
        }))
        .unwrap()
    }

    fn ids(v: &[&str]) -> Vec<Id> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn targets_skip_temporary_and_removed() {
        let p = project();
        let r = aiframes_core::resolve(&p);
        let t = target(&p, &r, &ids(&["b", "a"])).unwrap();
        assert_eq!((t.t0, t.t1), (10.0, 55.0));
        assert_eq!(t.tracks, ids(&["v2", "v1"]), "tracks in display order (top video first)");
        assert_eq!(t.items[0].t0, 10.0, "items in time order");
        assert_eq!(target(&p, &r, &ids(&["probe", "gone"])), None);
        let m = target(&p, &r, &ids(&["mk1"])).unwrap();
        assert_eq!((m.t0, m.t1, m.tracks.len()), (30.0, 30.0, 0));
        assert!(is_temporary(&[], "Temporary: checking the cut"));
        assert!(m.on_screen((0.0, 60.0)) && !t.on_screen((0.0, 50.0)) && !t.on_screen((12.0, 80.0)));
    }

    #[test]
    fn follow_keeps_the_persons_zoom() {
        let p = project();
        let r = aiframes_core::resolve(&p);
        let t = target(&p, &r, &ids(&["a", "b"])).unwrap(); // 10..55 s
        // Wide view, nobody touched it: show everything.
        assert!(matches!(plan(&t, (0.0, 100.0), false), Follow::InView(_)));
        assert!(matches!(plan(&t, (60.0, 160.0), false), Follow::All(_)));
        // The person zoomed into 12–30 s: never zoom out; the first item (10–12 s) is scrolled to.
        match plan(&t, (12.0, 30.0), true) {
            Follow::First(f) => assert_eq!((f.t0, f.t1, f.tracks.clone()), (10.0, 12.0, ids(&["v1"]))),
            other => panic!("{other:?}"),
        }
        // Wider than the view (even if nobody touched it): first item only, cut to the view width.
        match plan(&t, (100.0, 120.0), false) {
            Follow::First(f) => assert!(f.t0 == 10.0 && f.t1 <= 12.0),
            other => panic!("{other:?}"),
        }
        // First item already in view: nothing to scroll.
        assert!(matches!(plan(&t, (5.0, 25.0), true), Follow::InView(_)));
    }

    #[test]
    fn recap_of_a_session() {
        let before = project();
        let mut after = before.clone();
        after.clips.retain(|c| c.id != "b"); // removed
        after.clips[0].start = aiframes_core::Time::At(11.0); // "a" changed
        let mut n = after.clips[0].clone();
        n.id = "c9".into();
        after.clips.push(n); // added
        let [added, changed, removed, temporary] = session_recap(&before, &after, &ids(&["a", "c9", "tmp1"]));
        assert_eq!(added, ids(&["c9"]));
        assert_eq!(changed, ids(&["a"]));
        assert_eq!(removed, ids(&["b"]));
        assert_eq!(temporary, ids(&["tmp1"]));
    }
}
