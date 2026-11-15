//! Data-driven effect and transition registries. They drive the renderer, the UI inspector and the
//! MCP `list_effects` / `list_transitions` tools, so every parameter has a label, default, range,
//! unit and a one-line help.
//!
//! Unit vocabulary (`ParamSpec::unit`): `""` plain number, `"x"` multiplier, `"stops"` exposure stops,
//! `"px"` project pixels, `"deg"` degrees, `"frac"` fraction 0..1 (of the frame for positions/sizes),
//! `"toggle"` 0 = off / 1 = on, `"count"` whole number, `"ratio"` aspect ratio, `"hz"` per second.
//!
//! Positions in effects are fractions of the frame (0,0 = top-left, 1,1 = bottom-right).

use crate::eval::sample;
use crate::model::{Color, Effect, Transition};
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct ParamSpec {
    pub name: &'static str,
    pub label: &'static str,
    pub default: f64,
    pub min: f64,
    pub max: f64,
    pub unit: &'static str,
    pub help: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct EffectSpec {
    pub kind: &'static str,
    pub label: &'static str,
    /// "color", "blur", "stylize", "keying", "mask", "light", "distort", "motion".
    pub category: &'static str,
    pub params: &'static [ParamSpec],
    /// `(name, default "#RRGGBB[AA]")`.
    pub colors: &'static [(&'static str, &'static str)],
    /// Names of string parameters (e.g. a LUT path).
    pub strings: &'static [&'static str],
    pub help: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct TransitionSpec {
    pub kind: &'static str,
    pub label: &'static str,
    pub params: &'static [ParamSpec],
    pub help: &'static str,
    /// Suggested duration in seconds (used when `add_transition` omits `duration`).
    pub default_duration: f64,
    /// `(name, default "#RRGGBB[AA]")` colour parameters (set with `set_transition_colors`).
    pub colors: &'static [(&'static str, &'static str)],
}

const fn p(
    name: &'static str,
    label: &'static str,
    default: f64,
    min: f64,
    max: f64,
    unit: &'static str,
    help: &'static str,
) -> ParamSpec {
    ParamSpec { name, label, default, min, max, unit, help }
}

const MASK_X: ParamSpec = p("x", "Center X", 0.5, -1.0, 2.0, "frac", "Horizontal centre of the mask as a fraction of the frame width (0 = left edge).");
const MASK_Y: ParamSpec = p("y", "Center Y", 0.5, -1.0, 2.0, "frac", "Vertical centre of the mask as a fraction of the frame height (0 = top edge).");
const MASK_W: ParamSpec = p("width", "Width", 0.5, 0.0, 3.0, "frac", "Mask width as a fraction of the frame width.");
const MASK_H: ParamSpec = p("height", "Height", 0.5, 0.0, 3.0, "frac", "Mask height as a fraction of the frame height.");
const MASK_FEATHER: ParamSpec = p("feather", "Feather", 0.0, 0.0, 500.0, "px", "Soft edge width in pixels.");
const MASK_INVERT: ParamSpec = p("invert", "Invert", 0.0, 0.0, 1.0, "toggle", "1 keeps the outside of the shape instead of the inside.");

pub static EFFECTS: &[EffectSpec] = &[
    EffectSpec {
        kind: "color",
        label: "Color correction",
        category: "color",
        params: &[
            p("exposure", "Exposure", 0.0, -5.0, 5.0, "stops", "Brightens (+) or darkens (−) like camera exposure; +1 stop doubles the light."),
            p("contrast", "Contrast", 0.0, -1.0, 1.0, "", "Spreads (+) or flattens (−) tones around mid-grey; −1 is flat grey."),
            p("saturation", "Saturation", 1.0, 0.0, 3.0, "x", "Colour intensity multiplier: 0 = black and white, 1 = unchanged, 2 = double."),
            p("vibrance", "Vibrance", 0.0, -1.0, 1.0, "", "Boosts muted colours more than already-saturated ones (gentler than saturation)."),
            p("temperature", "Temperature", 0.0, -1.0, 1.0, "", "White balance: negative = cooler/bluer, positive = warmer/more orange."),
            p("tint", "Tint", 0.0, -1.0, 1.0, "", "White balance: negative = greener, positive = more magenta."),
            p("brightness", "Brightness", 0.0, -1.0, 1.0, "", "Adds (+) or subtracts (−) a flat amount of light to every pixel."),
            p("gamma", "Gamma", 1.0, 0.1, 5.0, "", "Mid-tone curve: above 1 brightens mid-tones, below 1 darkens them."),
            p("highlights", "Highlights", 0.0, -1.0, 1.0, "", "Recovers (−) or boosts (+) the brightest areas only."),
            p("shadows", "Shadows", 0.0, -1.0, 1.0, "", "Lifts (+) or crushes (−) the darkest areas only."),
            p("hue", "Hue shift", 0.0, -180.0, 180.0, "deg", "Rotates every hue around the colour wheel."),
        ],
        colors: &[],
        strings: &[],
        help: "Primary colour correction: exposure, contrast, saturation, white balance and tone ranges.",
    },
    EffectSpec {
        kind: "blur",
        label: "Gaussian blur",
        category: "blur",
        params: &[p("radius", "Radius", 10.0, 0.0, 200.0, "px", "Gaussian standard deviation (σ) in project px, like CSS blur(); 0 = sharp.")],
        colors: &[],
        strings: &[],
        help: "Gaussian blur of the clip (radius = σ, CSS blur() semantics); animate the radius for a focus pull.",
    },
    EffectSpec {
        kind: "sharpen",
        label: "Sharpen",
        category: "blur",
        params: &[
            p("amount", "Amount", 0.5, 0.0, 5.0, "", "Strength of the sharpening (unsharp mask)."),
            p("radius", "Radius", 1.5, 0.5, 10.0, "px", "Gaussian σ (project px) of the unsharp mask: the size of the details that get sharpened."),
        ],
        colors: &[],
        strings: &[],
        help: "Unsharp-mask sharpening for soft footage.",
    },
    EffectSpec {
        kind: "vignette",
        label: "Vignette",
        category: "light",
        params: &[
            p("amount", "Amount", 0.5, 0.0, 1.0, "frac", "How dark the corners get (1 = fully the vignette colour)."),
            p("size", "Size", 0.65, 0.0, 1.0, "frac", "Radius of the untouched centre as a fraction of the half-diagonal."),
            p("softness", "Softness", 0.5, 0.0, 1.0, "frac", "Width of the fall-off from the centre to the corners."),
        ],
        colors: &[("color", "#000000")],
        strings: &[],
        help: "Darkens (or tints) the frame edges to draw the eye to the centre.",
    },
    EffectSpec {
        kind: "chroma_key",
        label: "Chroma key",
        category: "keying",
        params: &[
            p("tolerance", "Tolerance", 0.3, 0.0, 1.0, "", "BT.601 CbCr distance from the key colour within which pixels become fully transparent (pure green is about 0.54 from grey)."),
            p("softness", "Softness", 0.1, 0.0, 1.0, "", "Extra CbCr distance beyond the tolerance over which pixels fade from transparent to opaque."),
            p("spill", "Spill suppression", 0.5, 0.0, 1.0, "frac", "Removes the key colour's tint reflected onto the subject."),
        ],
        colors: &[("key", "#00FF00")],
        strings: &[],
        help: "Makes a green (or any key colour) background transparent by its BT.601 chroma (CbCr) distance from the key colour; brightness does not matter.",
    },
    EffectSpec {
        kind: "mask_rect",
        label: "Rectangle mask",
        category: "mask",
        params: &[
            MASK_X,
            MASK_Y,
            MASK_W,
            MASK_H,
            MASK_FEATHER,
            p("radius", "Corner radius", 0.0, 0.0, 2000.0, "px", "Rounds the rectangle's corners."),
            MASK_INVERT,
        ],
        colors: &[],
        strings: &[],
        help: "Keeps only a (rounded) rectangle of the clip; animate x/y/width/height to reveal or track.",
    },
    EffectSpec {
        kind: "mask_ellipse",
        label: "Ellipse mask",
        category: "mask",
        params: &[MASK_X, MASK_Y, MASK_W, MASK_H, MASK_FEATHER, MASK_INVERT],
        colors: &[],
        strings: &[],
        help: "Keeps only an ellipse of the clip (spotlights, circular reveals).",
    },
    EffectSpec {
        kind: "drop_shadow",
        label: "Drop shadow",
        category: "light",
        params: &[
            p("distance", "Distance", 12.0, 0.0, 500.0, "px", "How far the shadow is offset from the content."),
            p("angle", "Angle", 45.0, -360.0, 360.0, "deg", "Direction the shadow falls: 0 = right, 90 = down."),
            p("blur", "Blur", 16.0, 0.0, 200.0, "px", "Soft edge width in px (2σ of the Gaussian); 0 = hard shadow."),
            p("opacity", "Opacity", 0.6, 0.0, 1.0, "frac", "Shadow strength."),
        ],
        colors: &[("color", "#000000")],
        strings: &[],
        help: "Casts a soft shadow behind the clip's non-transparent pixels (text, cut-outs, picture-in-picture).",
    },
    EffectSpec {
        kind: "glow",
        label: "Glow",
        category: "light",
        params: &[
            p("radius", "Radius", 24.0, 0.0, 300.0, "px", "How far the glow spreads: soft edge width in px (2σ of the Gaussian)."),
            p("intensity", "Intensity", 1.0, 0.0, 5.0, "x", "Brightness of the glow."),
            p("threshold", "Threshold", 0.6, 0.0, 1.0, "frac", "Only pixels brighter than this glow (0 = everything)."),
        ],
        colors: &[("color", "#FFFFFF")],
        strings: &[],
        help: "Blooms bright areas outward; the colour tints the glow (white keeps the image's own colours).",
    },
    EffectSpec {
        kind: "pixelate",
        label: "Pixelate",
        category: "stylize",
        params: &[p("size", "Block size", 16.0, 1.0, 512.0, "px", "Size of each square block.")],
        colors: &[],
        strings: &[],
        help: "Mosaic of square blocks (combine with a mask to censor part of the frame).",
    },
    EffectSpec {
        kind: "grain",
        label: "Film grain",
        category: "stylize",
        params: &[
            p("amount", "Amount", 0.15, 0.0, 1.0, "frac", "Grain strength."),
            p("size", "Size", 1.0, 0.5, 8.0, "px", "Grain particle size."),
            p("chroma", "Colour grain", 0.0, 0.0, 1.0, "toggle", "1 = coloured grain, 0 = monochrome."),
            p("seed", "Seed", 0.0, 0.0, 1_000_000.0, "count", "Changes the pattern; the same seed always renders the same grain (deterministic)."),
        ],
        colors: &[],
        strings: &[],
        help: "Deterministic film grain: the noise depends only on seed and frame time, so renders are repeatable.",
    },
    EffectSpec {
        kind: "lut",
        label: "LUT",
        category: "color",
        params: &[p("intensity", "Intensity", 1.0, 0.0, 1.0, "frac", "Mix between the original (0) and the graded look (1).")],
        colors: &[],
        strings: &["path"],
        help: "Applies a .cube 3D LUT; `path` is relative to the project folder or absolute.",
    },
    EffectSpec {
        kind: "black_white",
        label: "Black & white",
        category: "color",
        params: &[
            p("amount", "Amount", 1.0, 0.0, 1.0, "frac", "Mix between colour (0) and black and white (1)."),
            p("red", "Red weight", 0.30, -2.0, 3.0, "", "How bright reds become in the grey image."),
            p("green", "Green weight", 0.59, -2.0, 3.0, "", "How bright greens become in the grey image."),
            p("blue", "Blue weight", 0.11, -2.0, 3.0, "", "How bright blues become in the grey image."),
        ],
        colors: &[],
        strings: &[],
        help: "Converts to monochrome with adjustable channel mix.",
    },
    EffectSpec {
        kind: "invert",
        label: "Invert",
        category: "stylize",
        params: &[p("amount", "Amount", 1.0, 0.0, 1.0, "frac", "Mix between the original (0) and the negative (1).")],
        colors: &[],
        strings: &[],
        help: "Photographic negative.",
    },
    EffectSpec {
        kind: "posterize",
        label: "Posterize",
        category: "stylize",
        params: &[p("levels", "Levels", 6.0, 2.0, 64.0, "count", "Number of tone levels per colour channel.")],
        colors: &[],
        strings: &[],
        help: "Reduces the number of tones for a flat, graphic look.",
    },
    EffectSpec {
        kind: "letterbox",
        label: "Letterbox",
        category: "stylize",
        params: &[p("aspect", "Aspect ratio", 2.39, 0.5, 4.0, "ratio", "Visible picture aspect (2.39 = cinemascope bars on 16:9).")],
        colors: &[("color", "#000000")],
        strings: &[],
        help: "Adds bars to frame the picture at a cinematic aspect ratio.",
    },
    EffectSpec {
        kind: "zoom_blur",
        label: "Zoom blur",
        category: "blur",
        params: &[
            p("amount", "Amount", 0.3, 0.0, 1.0, "frac", "Length of the radial streaks."),
            p("x", "Center X", 0.5, 0.0, 1.0, "frac", "Horizontal centre of the zoom."),
            p("y", "Center Y", 0.5, 0.0, 1.0, "frac", "Vertical centre of the zoom."),
        ],
        colors: &[],
        strings: &[],
        help: "Radial motion blur from a centre point (impact moments, punch-ins).",
    },
    EffectSpec {
        kind: "rgb_split",
        label: "RGB split",
        category: "distort",
        params: &[
            p("amount", "Amount", 6.0, 0.0, 100.0, "px", "Distance between the red and blue channels."),
            p("angle", "Angle", 0.0, -360.0, 360.0, "deg", "Direction of the split: 0 = horizontal."),
        ],
        colors: &[],
        strings: &[],
        help: "Chromatic-aberration glitch: offsets the red and blue channels.",
    },
    EffectSpec {
        kind: "shake",
        label: "Camera shake",
        category: "motion",
        params: &[
            p("amount", "Amount", 12.0, 0.0, 200.0, "px", "Maximum displacement."),
            p("frequency", "Frequency", 8.0, 0.1, 60.0, "hz", "Shakes per second."),
            p("rotation", "Rotation", 0.0, 0.0, 20.0, "deg", "Maximum rotational shake."),
            p("seed", "Seed", 0.0, 0.0, 1_000_000.0, "count", "Changes the (deterministic) shake pattern."),
        ],
        colors: &[],
        strings: &[],
        help: "Deterministic handheld/impact shake (animate amount to 0 for a decaying hit).",
    },
    EffectSpec {
        kind: "directional_blur",
        label: "Directional blur",
        category: "blur",
        params: &[
            p("angle", "Angle", 0.0, -360.0, 360.0, "deg", "Direction of the blur: 0 = horizontal, 90 = vertical."),
            p("length", "Length", 20.0, 0.0, 500.0, "px", "Length of the smear along that direction."),
        ],
        colors: &[],
        strings: &[],
        help: "Motion-style blur along one direction (speed lines without moving the clip).",
    },
    EffectSpec {
        kind: "glitch",
        label: "Glitch",
        category: "stylize",
        params: &[
            p("amount", "Amount", 0.5, 0.0, 1.0, "frac", "How far slices jump and how often (0 = off)."),
            p("slices", "Slices", 12.0, 1.0, 100.0, "count", "Number of horizontal slices."),
            p("seed", "Seed", 0.0, 0.0, 1_000_000.0, "count", "Changes the (deterministic) pattern; it re-rolls every frame."),
            p("rgb", "RGB split", 8.0, 0.0, 100.0, "px", "Offset between the red and blue channels of displaced slices."),
        ],
        colors: &[],
        strings: &[],
        help: "Digital glitch: horizontal slices jump sideways with an RGB split (animate amount for bursts on beats).",
    },
    EffectSpec {
        kind: "echo",
        label: "Echo",
        category: "motion",
        params: &[
            p("count", "Echoes", 3.0, 1.0, 12.0, "count", "Number of trailing copies."),
            p("interval", "Interval", 0.05, 0.001, 5.0, "s", "Time between echoes: echo k shows the clip at t - k × interval."),
            p("decay", "Decay", 0.5, 0.0, 1.0, "frac", "Opacity of each echo relative to the previous one."),
        ],
        colors: &[],
        strings: &[],
        help: "Time trails: draws the clip as it was k × interval earlier, under itself, with decaying opacity.",
    },
    EffectSpec {
        kind: "stutter",
        label: "Stutter",
        category: "motion",
        params: &[
            p("rate", "Rate", 8.0, 0.1, 60.0, "hz", "Steps per second."),
            p("hold", "Hold", 0.5, 0.0, 1.0, "frac", "Part of each step frozen on the step's first frame (1 = pure frame repeat, 0 = off)."),
        ],
        colors: &[],
        strings: &[],
        help: "Frame-repeat stutter: quantises the clip's local time into steps (the picture and its animation hold, then jump).",
    },
    EffectSpec {
        kind: "edge_glow",
        label: "Edge glow",
        category: "light",
        params: &[
            p("threshold", "Threshold", 0.2, 0.0, 1.0, "frac", "Edge strength needed to glow (lower = more edges)."),
            p("radius", "Radius", 8.0, 0.0, 200.0, "px", "Size of the glow around edges."),
            p("intensity", "Intensity", 1.5, 0.0, 10.0, "x", "Brightness of the glow."),
        ],
        colors: &[("color", "#00E5FF")],
        strings: &[],
        help: "Glowing outlines: detects edges and adds a coloured glow around them.",
    },
    EffectSpec {
        kind: "lens_distortion",
        label: "Lens distortion",
        category: "distort",
        params: &[p("amount", "Amount", 0.2, -1.0, 1.0, "", "Positive = barrel (bulges out), negative = pincushion.")],
        colors: &[],
        strings: &[],
        help: "Barrel or pincushion lens distortion around the frame centre.",
    },
    EffectSpec {
        kind: "exposure_flash",
        label: "Exposure flash",
        category: "light",
        params: &[p("amount", "Amount", 0.5, 0.0, 4.0, "x", "White light added (keyframe 0 → 1 → 0 for a flash on a beat).")],
        colors: &[],
        strings: &[],
        help: "Additive brightness pulse; keyframe it to flash on beats.",
    },
];

const WIPE_SOFT: ParamSpec = p("softness", "Edge softness", 0.05, 0.0, 1.0, "frac", "Width of the soft wipe edge as a fraction of the frame.");
const DIP_HOLD: ParamSpec = p("hold", "Hold", 0.1, 0.0, 0.9, "frac", "Fraction of the transition spent fully on the dip colour.");
const ZOOM_AMOUNT: ParamSpec = p("amount", "Zoom", 1.5, 1.0, 10.0, "x", "Scale reached at the cut.");

pub static TRANSITIONS: &[TransitionSpec] = &[
    TransitionSpec { kind: "crossfade", label: "Cross dissolve", params: &[], help: "The outgoing picture fades into the incoming one; sound crossfades.", default_duration: 0.5, colors: &[] },
    TransitionSpec {
        kind: "dip_black",
        label: "Dip to black",
        params: &[DIP_HOLD],
        help: "Fades out to black, then in from black, within the union of both clips' alpha (the full frame for full-frame clips).",
        default_duration: 0.6,
        colors: &[],
    },
    TransitionSpec {
        kind: "dip_white",
        label: "Dip to white",
        params: &[DIP_HOLD],
        help: "Fades out to white, then in from white (a flash), within the union of both clips' alpha (the full frame for full-frame clips).",
        default_duration: 0.6,
        colors: &[],
    },
    TransitionSpec { kind: "wipe_left", label: "Wipe left", params: &[WIPE_SOFT], help: "A straight edge sweeps right-to-left revealing the incoming clip.", default_duration: 0.5, colors: &[] },
    TransitionSpec { kind: "wipe_right", label: "Wipe right", params: &[WIPE_SOFT], help: "A straight edge sweeps left-to-right revealing the incoming clip.", default_duration: 0.5, colors: &[] },
    TransitionSpec { kind: "wipe_up", label: "Wipe up", params: &[WIPE_SOFT], help: "A straight edge sweeps bottom-to-top revealing the incoming clip.", default_duration: 0.5, colors: &[] },
    TransitionSpec { kind: "wipe_down", label: "Wipe down", params: &[WIPE_SOFT], help: "A straight edge sweeps top-to-bottom revealing the incoming clip.", default_duration: 0.5, colors: &[] },
    TransitionSpec { kind: "slide_left", label: "Slide left", params: &[], help: "The incoming clip slides in from the right, over the outgoing one.", default_duration: 0.4, colors: &[] },
    TransitionSpec { kind: "slide_right", label: "Slide right", params: &[], help: "The incoming clip slides in from the left, over the outgoing one.", default_duration: 0.4, colors: &[] },
    TransitionSpec { kind: "slide_up", label: "Slide up", params: &[], help: "The incoming clip slides in from the bottom, over the outgoing one.", default_duration: 0.4, colors: &[] },
    TransitionSpec { kind: "slide_down", label: "Slide down", params: &[], help: "The incoming clip slides in from the top, over the outgoing one.", default_duration: 0.4, colors: &[] },
    TransitionSpec { kind: "push_left", label: "Push left", params: &[], help: "The incoming clip pushes the outgoing one off to the left.", default_duration: 0.4, colors: &[] },
    TransitionSpec { kind: "push_right", label: "Push right", params: &[], help: "The incoming clip pushes the outgoing one off to the right.", default_duration: 0.4, colors: &[] },
    TransitionSpec { kind: "push_up", label: "Push up", params: &[], help: "The incoming clip pushes the outgoing one off the top.", default_duration: 0.4, colors: &[] },
    TransitionSpec { kind: "push_down", label: "Push down", params: &[], help: "The incoming clip pushes the outgoing one off the bottom.", default_duration: 0.4, colors: &[] },
    TransitionSpec { kind: "zoom_in", label: "Zoom in", params: &[ZOOM_AMOUNT], help: "Punches into the outgoing clip and lands on the incoming one.", default_duration: 0.4, colors: &[] },
    TransitionSpec { kind: "zoom_out", label: "Zoom out", params: &[ZOOM_AMOUNT], help: "The incoming clip starts zoomed in and settles to full frame.", default_duration: 0.4, colors: &[] },
    TransitionSpec {
        kind: "iris",
        label: "Iris",
        params: &[
            p("softness", "Edge softness", 0.05, 0.0, 1.0, "frac", "Width of the soft circle edge as a fraction of the frame."),
            p("x", "Center X", 0.5, 0.0, 1.0, "frac", "Horizontal centre of the circle."),
            p("y", "Center Y", 0.5, 0.0, 1.0, "frac", "Vertical centre of the circle."),
        ],
        help: "A growing circle reveals the incoming clip.",
        default_duration: 0.6,
        colors: &[],
    },
    TransitionSpec {
        kind: "blur_dissolve",
        label: "Blur dissolve",
        params: &[p("radius", "Blur radius", 30.0, 0.0, 200.0, "px", "Peak blur at the middle of the transition.")],
        help: "Blurs out of the outgoing clip and into the incoming one while dissolving.",
        default_duration: 0.5,
        colors: &[],
    },
    TransitionSpec {
        kind: "whip",
        label: "Whip pan",
        params: &[
            p("blur", "Motion blur", 0.8, 0.0, 1.0, "frac", "Strength of the directional motion blur."),
            p("angle", "Direction", 0.0, -360.0, 360.0, "deg", "Direction the picture travels: 0 = to the left (new clip enters from the right)."),
        ],
        help: "Fast motion-blurred push, like a whip pan between shots.",
        default_duration: 0.3,
        colors: &[],
    },
    TransitionSpec {
        kind: "spin",
        label: "Spin",
        params: &[
            p("turns", "Turns", 1.0, 0.25, 5.0, "", "Full rotations over the transition (the outgoing clip spins out, the incoming one spins in)."),
            p("direction", "Direction", 1.0, -1.0, 1.0, "", "1 = clockwise, -1 = counter-clockwise."),
            p("blur", "Spin blur", 0.6, 0.0, 1.0, "frac", "Strength of the rotational motion blur."),
        ],
        help: "Spins the picture through the cut with rotational motion blur (edit-style spin transition).",
        default_duration: 0.4,
        colors: &[],
    },
    TransitionSpec {
        kind: "zoom_through",
        label: "Zoom through",
        params: &[
            p("amount", "Zoom", 3.0, 1.0, 20.0, "x", "Scale the outgoing clip punches into, and the incoming one zooms out from."),
            p("blur", "Zoom blur", 0.6, 0.0, 1.0, "frac", "Strength of the radial (zoom) blur at the cut."),
        ],
        help: "Punches through the outgoing clip with a radial blur and lands on the incoming one zooming out.",
        default_duration: 0.4,
        colors: &[],
    },
    TransitionSpec {
        kind: "shake",
        label: "Shake",
        params: &[
            p("amount", "Amount", 40.0, 0.0, 500.0, "px", "Peak displacement at the cut."),
            p("angle", "Angle", 0.0, -360.0, 360.0, "deg", "Direction of the shake: 0 = horizontal, 90 = vertical."),
            p("blur", "Motion blur", 0.5, 0.0, 1.0, "frac", "Directional blur along the shake."),
        ],
        help: "A hard camera shake that peaks at the cut, with directional motion blur.",
        default_duration: 0.3,
        colors: &[],
    },
    TransitionSpec {
        kind: "glitch",
        label: "Glitch",
        params: &[
            p("amount", "Amount", 0.7, 0.0, 1.0, "frac", "Strength of the slice displacement and RGB split at the cut."),
            p("slices", "Slices", 16.0, 1.0, 100.0, "count", "Number of horizontal slices that jump sideways."),
        ],
        help: "Digital glitch through the cut: horizontal slices jump and the RGB channels split, switching clips mid-glitch.",
        default_duration: 0.3,
        colors: &[],
    },
    TransitionSpec {
        kind: "flash_zoom",
        label: "Flash zoom",
        params: &[p("amount", "Zoom", 1.3, 1.0, 5.0, "x", "Zoom punch at the cut.")],
        help: "A white flash at the cut with a zoom punch into the incoming clip.",
        default_duration: 0.4,
        colors: &[],
    },
    TransitionSpec {
        kind: "film_burn",
        label: "Film burn",
        params: &[p("seed", "Seed", 0.0, 0.0, 1_000_000.0, "count", "Changes the (deterministic) burn pattern.")],
        help: "An organic light leak / film burn in the colour sweeps over the cut and fades away on the incoming clip.",
        default_duration: 0.8,
        colors: &[("color", "#FF8A2B")],
    },
    TransitionSpec {
        kind: "cube",
        label: "Cube",
        params: &[p("direction", "Direction", 0.0, 0.0, 3.0, "count", "Way the cube turns: 0 = left, 1 = right, 2 = up, 3 = down.")],
        help: "The two clips are faces of a 3-D cube that rotates to show the incoming one.",
        default_duration: 0.6,
        colors: &[],
    },
    TransitionSpec {
        kind: "flip",
        label: "Flip",
        params: &[p("axis", "Axis", 1.0, 0.0, 1.0, "count", "0 = flips over the horizontal (x) axis, 1 = turns around the vertical (y) axis.")],
        help: "The picture flips like a card in 3-D; its back is the incoming clip.",
        default_duration: 0.5,
        colors: &[],
    },
    TransitionSpec {
        kind: "slide_blur",
        label: "Slide blur",
        params: &[p("direction", "Direction", 0.0, 0.0, 3.0, "count", "Way the pictures travel: 0 = left, 1 = right, 2 = up, 3 = down.")],
        help: "Both clips slide fast in one direction with strong motion blur (a smooth whip).",
        default_duration: 0.35,
        colors: &[],
    },
    TransitionSpec {
        kind: "stretch",
        label: "Stretch",
        params: &[p("direction", "Direction", 0.0, 0.0, 3.0, "count", "Way the picture stretches: 0 = left, 1 = right, 2 = up, 3 = down.")],
        help: "The outgoing picture stretches and smears in the direction, then the incoming one snaps back from the opposite stretch.",
        default_duration: 0.35,
        colors: &[],
    },
    TransitionSpec {
        kind: "luma_wipe",
        label: "Luma wipe",
        params: &[
            p("softness", "Edge softness", 0.1, 0.0, 1.0, "frac", "Width of the soft edge in luminance."),
            p("invert", "Invert", 0.0, 0.0, 1.0, "toggle", "1 = dark areas of the incoming clip appear first."),
        ],
        help: "Wipes by the incoming clip's luminance: its bright areas appear first (dark ones with invert).",
        default_duration: 0.8,
        colors: &[],
    },
];

const CROP_HELP: &str = "Crop applies before fit: media is re-fitted after cropping, a solid's box just shrinks; text and shapes ignore crop.";

/// The clip-level parameters (`set_param` paths) with defaults, ranges and help, for inspectors and the
/// MCP. Units as for effects plus `"s"` seconds. `transform.anchor_*` and the fades are not animatable.
pub static CLIP_PARAMS: &[ParamSpec] = &[
    p("opacity", "Opacity", 1.0, 0.0, 1.0, "frac", "Picture opacity, multiplied by the fades (and the mix amount of an adjustment clip)."),
    p("volume", "Volume", 1.0, 0.0, 16.0, "x", "Linear gain of the clip's sound (1 = unchanged, 0.5 ≈ −6 dB); multiplied by the fades, the track volume and ducking."),
    p("speed", "Speed", 1.0, 0.0, 100.0, "x", "Playback speed: 1 normal, 2 double, 0.5 slow motion, 0 freeze. Keys make a speed ramp (the source position is the integral of speed)."),
    p("transform.x", "Position X", 0.0, -20000.0, 20000.0, "px", "Horizontal offset of the content's centre from the frame centre (+ = right)."),
    p("transform.y", "Position Y", 0.0, -20000.0, 20000.0, "px", "Vertical offset of the content's centre from the frame centre (+ = down)."),
    p("transform.scale", "Scale", 1.0, 0.0, 50.0, "x", "Uniform scale on top of fit (1 = fitted size). Text and shapes ignore fit and draw at their own size."),
    p("transform.rotation", "Rotation", 0.0, -3600.0, 3600.0, "deg", "Clockwise rotation around the anchor point."),
    p("transform.rotation_x", "Tilt (3-D X)", 0.0, -3600.0, 3600.0, "deg", "3-D card tilt around the horizontal axis through the anchor (+ = top edge away)."),
    p("transform.rotation_y", "Turn (3-D Y)", 0.0, -3600.0, 3600.0, "deg", "3-D card turn around the vertical axis through the anchor (+ = right edge away)."),
    p("transform.perspective", "Perspective", 2000.0, 0.0, 100_000.0, "px", "Focal distance of the 3-D view (smaller = stronger perspective; 0 = orthographic). Not animatable."),
    p("transform.anchor_x", "Anchor X", 0.5, 0.0, 1.0, "frac", "Pivot for scale and rotation as a fraction of the content box (0.5 = centre). Not animatable."),
    p("transform.anchor_y", "Anchor Y", 0.5, 0.0, 1.0, "frac", "Pivot for scale and rotation as a fraction of the content box (0.5 = centre). Not animatable."),
    p("transform.crop.left", "Crop left", 0.0, 0.0, 1.0, "frac", CROP_HELP),
    p("transform.crop.top", "Crop top", 0.0, 0.0, 1.0, "frac", CROP_HELP),
    p("transform.crop.right", "Crop right", 0.0, 0.0, 1.0, "frac", CROP_HELP),
    p("transform.crop.bottom", "Crop bottom", 0.0, 0.0, 1.0, "frac", CROP_HELP),
    p("fade_in", "Fade in", 0.0, 0.0, 3600.0, "s", "Seconds over which picture (opacity) and sound rise from the clip's start. Not animatable."),
    p("fade_out", "Fade out", 0.0, 0.0, 3600.0, "s", "Seconds over which picture (opacity) and sound fall to the clip's end. Not animatable."),
    p("source.draw", "Draw on", 1.0, 0.0, 1.0, "frac", "Portion of a shape's outline drawn; animate 0 → 1 for a draw-on."),
];

/// Ducking settings with defaults, ranges and help (units `"dB"`, `"dBFS"`, `"s"`).
pub static DUCKING_PARAMS: &[ParamSpec] = &[
    p("amount_db", "Reduction", -14.0, -60.0, 0.0, "dB", "Gain change of the target track while the trigger track is active."),
    p("threshold_db", "Threshold", -45.0, -100.0, 0.0, "dBFS", "Trigger level above which the trigger counts as active."),
    p("attack", "Attack", 0.12, 0.0, 10.0, "s", "Fade-down time. The ducker looks ahead by `attack`, so the target is already down when the trigger starts."),
    p("release", "Release", 0.45, 0.0, 10.0, "s", "Fade-up time, starting after the trigger has stayed below the threshold for 0.3 s (hold)."),
];

/// The spec of a clip-level parameter path (`"opacity"`, `"transform.crop.left"`, …); effect parameters
/// (`effects.N.<param>`) are described by [`effect_spec`].
pub fn clip_param_spec(path: &str) -> Option<&'static ParamSpec> {
    CLIP_PARAMS.iter().find(|p| p.name == path)
}

pub fn effect_spec(kind: &str) -> Option<&'static EffectSpec> {
    EFFECTS.iter().find(|e| e.kind == kind)
}

pub fn transition_spec(kind: &str) -> Option<&'static TransitionSpec> {
    TRANSITIONS.iter().find(|t| t.kind == kind)
}

impl EffectSpec {
    pub fn param(&self, name: &str) -> Option<&'static ParamSpec> {
        self.params.iter().find(|p| p.name == name)
    }
    pub fn color_default(&self, name: &str) -> Option<Color> {
        self.colors.iter().find(|(n, _)| *n == name).and_then(|(_, c)| Color::parse(c))
    }
    /// A new effect of this kind with every parameter at its default (defaults are implied, so the
    /// maps stay empty).
    pub fn instantiate(&self) -> Effect {
        Effect {
            kind: self.kind.to_string(),
            enabled: true,
            params: Default::default(),
            colors: Default::default(),
            strings: Default::default(),
        }
    }
}

impl TransitionSpec {
    pub fn param(&self, name: &str) -> Option<&'static ParamSpec> {
        self.params.iter().find(|p| p.name == name)
    }
    pub fn color_default(&self, name: &str) -> Option<Color> {
        self.colors.iter().find(|(n, _)| *n == name).and_then(|(_, c)| Color::parse(c))
    }
}

/// An effect parameter at clip-local time: the clip's value, else the registry default, else 0.
pub fn effect_param(e: &Effect, name: &str, t_local: f64) -> f64 {
    if let Some(p) = e.params.get(name) {
        return sample(p, t_local);
    }
    effect_spec(&e.kind).and_then(|s| s.param(name)).map(|p| p.default).unwrap_or(0.0)
}

/// An effect colour: the clip's value, else the registry default, else black.
pub fn effect_color(e: &Effect, name: &str) -> Color {
    if let Some(c) = e.colors.get(name) {
        return *c;
    }
    effect_spec(&e.kind).and_then(|s| s.color_default(name)).unwrap_or(Color::BLACK)
}

/// An effect string parameter (empty when unset).
pub fn effect_string<'a>(e: &'a Effect, name: &str) -> &'a str {
    e.strings.get(name).map(String::as_str).unwrap_or("")
}

/// A transition parameter: the transition's value, else the registry default, else 0.
pub fn transition_param(t: &Transition, name: &str) -> f64 {
    if let Some(v) = t.params.get(name) {
        return *v;
    }
    transition_spec(&t.kind).and_then(|s| s.param(name)).map(|p| p.default).unwrap_or(0.0)
}

/// A transition colour: the transition's value, else the registry default, else white.
pub fn transition_color(t: &Transition, name: &str) -> Color {
    if let Some(c) = t.colors.get(name) {
        return *c;
    }
    transition_spec(&t.kind).and_then(|s| s.color_default(name)).unwrap_or(Color::WHITE)
}

/// Every effect kind name.
pub fn effect_kinds() -> impl Iterator<Item = &'static str> {
    EFFECTS.iter().map(|e| e.kind)
}

/// Every transition kind name.
pub fn transition_kinds() -> impl Iterator<Item = &'static str> {
    TRANSITIONS.iter().map(|t| t.kind)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Param;
    use std::collections::HashSet;

    #[test]
    fn registry_is_complete_and_consistent() {
        let required = [
            "color", "blur", "sharpen", "vignette", "chroma_key", "mask_rect", "mask_ellipse", "drop_shadow", "glow",
            "pixelate", "grain", "lut", "black_white", "invert", "posterize", "letterbox", "zoom_blur",
        ];
        for k in required {
            assert!(effect_spec(k).is_some(), "missing effect {k}");
        }
        let color = effect_spec("color").unwrap();
        for n in ["exposure", "contrast", "saturation", "temperature", "tint", "brightness", "gamma", "highlights", "shadows"] {
            assert!(color.param(n).is_some(), "color.{n}");
        }
        assert_eq!(effect_spec("lut").unwrap().strings, &["path"]);
        let trans = [
            "crossfade", "dip_black", "dip_white", "wipe_left", "wipe_right", "wipe_up", "wipe_down", "slide_left",
            "slide_right", "slide_up", "slide_down", "push_left", "push_right", "zoom_in", "zoom_out", "iris",
            "blur_dissolve", "whip",
        ];
        for k in trans {
            assert!(transition_spec(k).is_some(), "missing transition {k}");
        }
        let mut kinds = HashSet::new();
        for e in EFFECTS {
            assert!(kinds.insert(e.kind), "duplicate effect {}", e.kind);
            assert!(!e.help.is_empty() && !e.label.is_empty());
            let mut names = HashSet::new();
            for p in e.params {
                assert!(names.insert(p.name), "{}.{} duplicated", e.kind, p.name);
                assert!(p.min <= p.default && p.default <= p.max, "{}.{} default out of range", e.kind, p.name);
                assert!(!p.help.is_empty() && !p.label.is_empty());
            }
            for (n, c) in e.colors {
                assert!(Color::parse(c).is_some(), "{}.{n}", e.kind);
            }
        }
        for t in TRANSITIONS {
            assert!(t.default_duration > 0.0);
            for p in t.params {
                assert!(p.min <= p.default && p.default <= p.max, "{}.{}", t.kind, p.name);
            }
        }
    }

    #[test]
    fn help_texts_describe_the_renderer() {
        let help = |k: &str, n: &str| effect_spec(k).unwrap().param(n).unwrap().help;
        assert!(help("blur", "radius").contains("standard deviation") && help("blur", "radius").contains("CSS blur()"));
        assert!(help("sharpen", "radius").contains("σ"));
        assert!(help("drop_shadow", "blur").contains("2σ") && help("glow", "radius").contains("2σ"));
        assert!(help("chroma_key", "tolerance").contains("BT.601 CbCr") && help("chroma_key", "tolerance").contains("0.54"));
        assert!(transition_spec("dip_black").unwrap().help.contains("union of both clips' alpha"));
        assert!(transition_spec("dip_white").unwrap().help.contains("union of both clips' alpha"));
        let crop = clip_param_spec("transform.crop.left").unwrap();
        assert!(crop.help.contains("before fit") && crop.help.contains("text and shapes ignore crop"));
        for p in CLIP_PARAMS.iter().chain(DUCKING_PARAMS) {
            assert!(p.min <= p.default && p.default <= p.max, "{}", p.name);
            assert!(!p.help.is_empty() && !p.label.is_empty());
        }
        // The ducking registry matches the model's serde defaults.
        let d: crate::model::Ducking = serde_json::from_str(r#"{"id":"d1","target":"a2","trigger":"a1"}"#).unwrap();
        let dv = |n: &str| DUCKING_PARAMS.iter().find(|p| p.name == n).unwrap().default;
        assert_eq!((d.amount_db, d.threshold_db, d.attack, d.release), (dv("amount_db"), dv("threshold_db"), dv("attack"), dv("release")));
        assert!(DUCKING_PARAMS.iter().find(|p| p.name == "release").unwrap().help.contains("0.3 s"));
    }

    #[test]
    fn values_fall_back_to_defaults() {
        let mut e = effect_spec("vignette").unwrap().instantiate();
        assert_eq!(effect_param(&e, "amount", 0.0), 0.5);
        assert_eq!(effect_param(&e, "nope", 0.0), 0.0);
        assert_eq!(effect_color(&e, "color"), Color::BLACK);
        e.params.insert("amount".into(), Param::Value(0.9));
        e.colors.insert("color".into(), Color::WHITE);
        assert_eq!(effect_param(&e, "amount", 3.0), 0.9);
        assert_eq!(effect_color(&e, "color"), Color::WHITE);
        let t = Transition { id: "tr1".into(), from: "a".into(), to: "b".into(), kind: "iris".into(), duration: 0.5, params: Default::default(), colors: Default::default() };
        assert_eq!(transition_param(&t, "x"), 0.5);
        let json = serde_json::to_value(EFFECTS).unwrap();
        assert!(json.as_array().unwrap().len() >= 17);
    }
}
