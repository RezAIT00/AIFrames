//! A per-render index of the project: resolved clips per track, valid transitions, assets.

use aiframes_core::effects::transition_spec;
use aiframes_core::{Clip, MediaAsset, Project, Resolved, Source, Span, Track, TrackKind, Transition};
use std::collections::HashMap;

/// Cuts closer than this count as touching.
const CUT_EPS: f64 = 1e-3;

pub(crate) struct TransInfo<'a> {
    pub tr: &'a Transition,
    /// Indices into the track's `clips`.
    pub from: usize,
    pub to: usize,
    pub cut: f64,
    /// Half the (clamped) duration: the window is `[cut - half, cut + half)`.
    pub half: f64,
}

impl TransInfo<'_> {
    pub fn start(&self) -> f64 {
        self.cut - self.half
    }
    pub fn end(&self) -> f64 {
        self.cut + self.half
    }
    /// Linear progress 0..1 at `t`.
    pub fn progress(&self, t: f64) -> f64 {
        ((t - self.start()) / (2.0 * self.half)).clamp(0.0, 1.0)
    }
}

pub(crate) struct TrackInfo<'a> {
    pub track: &'a Track,
    /// Resolved clips sorted by start.
    pub clips: Vec<(&'a Clip, Span)>,
    pub trans: Vec<TransInfo<'a>>,
}

pub(crate) struct Timeline<'a> {
    pub p: &'a Project,
    /// Time tolerance matching `frame_index` (1e-6 of a frame): a clip starting at
    /// 0.30000000000000004 covers the frame at 0.3.
    pub tol: f64,
    pub assets: HashMap<&'a str, &'a MediaAsset>,
    pub tracks: Vec<TrackInfo<'a>>,
    pub solo_video: bool,
    pub solo_any: bool,
    /// Transitions that could not be used (not adjacent, unknown, ...), with the reason.
    pub problems: Vec<String>,
    /// Every resolved clip by id (mattes look up their source).
    pub by_id: HashMap<&'a str, (&'a Clip, Span)>,
    /// Clips used as some clip's track matte: not drawn themselves.
    pub matte_sources: std::collections::HashSet<String>,
}

/// Whether span `s` holds frame time `t` (start inclusive, end exclusive, within `tol`).
pub(crate) fn covers(s: &Span, t: f64, tol: f64) -> bool {
    t + tol >= s.start && t + tol < s.end
}

/// Clip-local time at `t`, with float noise around the clip's first frame snapped to 0.
pub(crate) fn local_time(t: f64, s: &Span, tol: f64) -> f64 {
    let l = t - s.start;
    if l.abs() <= 2.0 * tol {
        0.0
    } else {
        l
    }
}

/// One thing to draw on a track at an instant.
pub(crate) enum Active<'a, 'b> {
    One(&'a Clip, Span),
    Pair { info: &'b TransInfo<'a>, a: (&'a Clip, Span), b: (&'a Clip, Span), p: f64 },
}

impl<'a> Timeline<'a> {
    pub fn new(p: &'a Project, r: &Resolved) -> Timeline<'a> {
        let assets = p.media.iter().map(|m| (m.id.as_str(), m)).collect();
        let mut by_track: HashMap<&str, Vec<(&Clip, Span)>> = HashMap::new();
        for c in &p.clips {
            if let Some(s) = r.span(&c.id) {
                if s.end > s.start && s.start.is_finite() && s.end.is_finite() {
                    by_track.entry(c.track.as_str()).or_default().push((c, s));
                }
            }
        }
        let mut problems = Vec::new();
        let mut tracks = Vec::new();
        for tr in &p.tracks {
            let mut clips = by_track.remove(tr.id.as_str()).unwrap_or_default();
            clips.sort_by(|a, b| a.1.start.total_cmp(&b.1.start).then(a.1.end.total_cmp(&b.1.end)));
            let index: HashMap<&str, usize> = clips.iter().enumerate().map(|(i, (c, _))| (c.id.as_str(), i)).collect();
            let mut trans = Vec::new();
            for t in &p.transitions {
                let (Some(&fi), Some(&ti)) = (index.get(t.from.as_str()), index.get(t.to.as_str())) else { continue };
                let (fs, ts) = (clips[fi].1, clips[ti].1);
                if (fs.end - ts.start).abs() > CUT_EPS {
                    problems.push(format!("transition {} ignored: {} does not end where {} starts", t.id, t.from, t.to));
                    continue;
                }
                if !(t.duration.is_finite() && t.duration > 0.0) {
                    continue;
                }
                if transition_spec(&t.kind).is_none() {
                    problems.push(format!("transition {} has unknown kind `{}`; rendered as a crossfade", t.id, t.kind));
                }
                let half = (t.duration / 2.0).min(fs.duration()).min(ts.duration());
                if half <= 0.0 {
                    continue;
                }
                trans.push(TransInfo { tr: t, from: fi, to: ti, cut: ts.start, half });
            }
            tracks.push(TrackInfo { track: tr, clips, trans });
        }
        let solo_video = p.tracks.iter().any(|t| t.kind == TrackKind::Video && t.solo);
        let solo_any = p.tracks.iter().any(|t| t.solo);
        let tol = 1e-6 * crate::util::frame_time(1, p.settings.fps);
        let by_id = tracks.iter().flat_map(|t| t.clips.iter().map(|(c, s)| (c.id.as_str(), (*c, *s)))).collect();
        let matte_sources = aiframes_core::matte_sources(p);
        Timeline { p, assets, tracks, solo_video, solo_any, problems, tol, by_id, matte_sources }
    }

    /// Whether a video track's picture is shown.
    pub fn shows_picture(&self, tr: &Track) -> bool {
        tr.kind == TrackKind::Video && !tr.hidden && (!self.solo_video || tr.solo)
    }

    /// Whether a track's sound is heard.
    pub fn plays_sound(&self, tr: &Track) -> bool {
        !tr.muted && (!self.solo_any || tr.solo)
    }

    /// What a track shows at `t`, in drawing order.
    pub fn active_at<'b>(&'b self, ti: usize, t: f64) -> Vec<Active<'a, 'b>> {
        let info = &self.tracks[ti];
        // The transition whose window holds `t` (nearest cut wins when windows overlap).
        let pair = info
            .trans
            .iter()
            .filter(|x| t + self.tol >= x.start() && t + self.tol < x.end())
            .filter(|x| info.clips[x.from].0.enabled && info.clips[x.to].0.enabled)
            .filter(|x| !matches!(info.clips[x.from].0.source, Source::Adjustment) && !matches!(info.clips[x.to].0.source, Source::Adjustment))
            .min_by(|a, b| (t - a.cut).abs().total_cmp(&(t - b.cut).abs()));
        let mut out = Vec::new();
        let mut pair_done = false;
        for (i, (c, s)) in info.clips.iter().enumerate() {
            if let Some(x) = pair {
                if i == x.from || i == x.to {
                    if !pair_done {
                        out.push(Active::Pair { info: x, a: info.clips[x.from], b: info.clips[x.to], p: x.progress(t) });
                        pair_done = true;
                    }
                    continue;
                }
            }
            if c.enabled && covers(s, t, self.tol) {
                out.push(Active::One(c, *s));
            }
        }
        out
    }
}
