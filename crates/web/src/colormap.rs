//! Colormaps the palette editor samples colors from.

use glurmo_model::theme::Color;

/// The colormaps by name, each as stops from dark to bright.
pub const COLORMAPS: [(&str, &[&str]); 4] = [
    (
        "viridis",
        &[
            "#440154", "#482878", "#3e4989", "#31688e", "#26828e", "#1f9e89", "#35b779", "#6ece58",
            "#b5de2b", "#fde725",
        ],
    ),
    (
        "magma",
        &[
            "#1d1147", "#51127c", "#822681", "#b63679", "#e65164", "#fb8861", "#fec287", "#fcfdbf",
        ],
    ),
    (
        "cividis",
        &[
            "#00224e", "#123570", "#3b496c", "#575d6d", "#707173", "#8a8678", "#a59c74", "#c3b369",
            "#e1cc55", "#fee838",
        ],
    ),
    (
        "batlow",
        &[
            "#011959", "#103f60", "#1c5a62", "#3c6d56", "#687b3e", "#9d892b", "#d29343", "#f8a17b",
            "#fdb7bc", "#faccfa",
        ],
    ),
];

/// `count` colors spread evenly over the colormap `name`, brightest first. None for a name not in
/// [`COLORMAPS`].
pub fn sampled(name: &str, count: u16) -> Vec<Color> {
    let Some((_, stops)) = COLORMAPS.iter().find(|(map, _)| *map == name) else {
        return Vec::new();
    };
    let stops: Vec<[f64; 3]> = stops
        .iter()
        .filter_map(|stop| Color::try_from((*stop).to_owned()).ok())
        .map(|stop| {
            let (r, g, b) = stop.rgb();
            [f64::from(r), f64::from(g), f64::from(b)]
        })
        .collect();
    let last = f64::from(u16::try_from(stops.len() - 1).unwrap_or(0));
    (0..count)
        .rev()
        .filter_map(|step| {
            let at = if count > 1 {
                f64::from(step) / f64::from(count - 1) * last
            } else {
                last
            };
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "a position between the first and last stop"
            )]
            let low = at.floor() as usize;
            let high = (low + 1).min(stops.len() - 1);
            let mix = at - at.floor();
            let channel = |i: usize| (stops[low][i] * (1.0 - mix) + stops[high][i] * mix).round();
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "a mix of two bytes stays a byte"
            )]
            let hex = format!(
                "#{:02x}{:02x}{:02x}",
                channel(0) as u8,
                channel(1) as u8,
                channel(2) as u8
            );
            Color::try_from(hex).ok()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_run_from_the_bright_end_to_the_dark_end() {
        let hex =
            |colors: Vec<Color>| -> Vec<String> { colors.into_iter().map(String::from).collect() };
        assert_eq!(hex(sampled("viridis", 2)), ["#fde725", "#440154"]);
        assert_eq!(hex(sampled("viridis", 1)), ["#fde725"]);
        assert_eq!(sampled("viridis", 7).len(), 7);
        assert_eq!(sampled("viridis", 0), []);
        assert_eq!(sampled("no-such-map", 3), []);
    }
}
