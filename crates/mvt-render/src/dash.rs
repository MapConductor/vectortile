//! Splitting a line into the runs a dash pattern keeps.
//!
//! `line-dasharray` is how a style says "this road is in a tunnel" or "this
//! boundary is disputed" -- 41 layers of the sample basemap use it, and drawn
//! solid they all read as something else.
//!
//! The lengths are multiples of the line's own width, not pixels, so a dash
//! pattern keeps its proportions as a road thickens with zoom. Callers scale
//! the pattern before handing it over, which is also what lets the same code
//! serve the pixel-space rasteriser and the extent-space tessellator.

/// Turns an odd-length pattern into the even one it means.
///
/// `[3]` is three on, three off, forever -- the same rule SVG uses. An empty
/// or non-positive pattern means no dashing at all.
pub fn normalise(pattern: &[f32]) -> Vec<f32> {
    let cleaned: Vec<f32> = pattern.iter().copied().filter(|v| *v >= 0.0).collect();
    if cleaned.is_empty() || cleaned.iter().all(|v| *v <= 0.0) {
        return Vec::new();
    }
    if cleaned.len().is_multiple_of(2) {
        cleaned
    } else {
        let mut doubled = cleaned.clone();
        doubled.extend_from_slice(&cleaned);
        doubled
    }
}

/// The parts of `points` a dash pattern draws, each a polyline of its own.
///
/// The pattern runs continuously along the whole line rather than restarting
/// at every vertex: a dash that met a corner would otherwise be cut in two and
/// the gaps would bunch up wherever the geometry happened to be dense.
pub fn dashed(points: &[[f32; 2]], pattern: &[f32]) -> Vec<Vec<[f32; 2]>> {
    let pattern = normalise(pattern);
    if pattern.is_empty() || points.len() < 2 {
        return Vec::new();
    }

    let mut runs: Vec<Vec<[f32; 2]>> = Vec::new();
    let mut current: Vec<[f32; 2]> = Vec::new();

    // Where we are in the pattern: which entry, and how much of it is left.
    let mut index = 0usize;
    let mut left = pattern[0];
    let mut drawing = true;

    if drawing {
        current.push(points[0]);
    }

    for pair in points.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
        let length = dx.hypot(dy);
        if length <= f32::EPSILON {
            continue;
        }

        let mut travelled = 0.0f32;
        while length - travelled > left {
            travelled += left;
            let t = travelled / length;
            let cut = [a[0] + dx * t, a[1] + dy * t];
            if drawing {
                current.push(cut);
                runs.push(std::mem::take(&mut current));
            } else {
                current.clear();
                current.push(cut);
            }
            drawing = !drawing;
            index = (index + 1) % pattern.len();
            left = pattern[index];
            // A zero-length entry would spin here forever.
            if left <= 0.0 {
                left = f32::EPSILON;
            }
        }

        left -= length - travelled;
        if drawing {
            current.push(b);
        }
    }

    if drawing && current.len() > 1 {
        runs.push(current);
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(points: &[(f32, f32)]) -> Vec<[f32; 2]> {
        points.iter().map(|(x, y)| [*x, *y]).collect()
    }

    fn length(run: &[[f32; 2]]) -> f32 {
        run.windows(2)
            .map(|p| (p[1][0] - p[0][0]).hypot(p[1][1] - p[0][1]))
            .sum()
    }

    #[test]
    fn cuts_a_straight_line_into_dashes() {
        let runs = dashed(&line(&[(0.0, 0.0), (100.0, 0.0)]), &[10.0, 10.0]);
        // 10 on, 10 off, across 100: five dashes.
        assert_eq!(runs.len(), 5);
        for run in &runs {
            assert!((length(run) - 10.0).abs() < 0.01, "got {run:?}");
        }
        assert_eq!(runs[0][0], [0.0, 0.0]);
        assert_eq!(runs[0][1], [10.0, 0.0]);
        assert_eq!(runs[1][0], [20.0, 0.0]);
    }

    /// The pattern belongs to the line, not to each segment. Restarting it at
    /// every vertex bunches the gaps wherever the geometry is dense -- which
    /// on a curve is everywhere.
    #[test]
    fn carries_the_pattern_across_a_corner() {
        let runs = dashed(
            &line(&[(0.0, 0.0), (15.0, 0.0), (15.0, 15.0)]),
            &[10.0, 10.0],
        );
        // First dash 0..10 along the top. Then a gap to 20, which is five
        // past the corner, so the next dash starts down the side.
        assert_eq!(runs.len(), 2);
        assert!((length(&runs[0]) - 10.0).abs() < 0.01);
        assert_eq!(runs[1][0], [15.0, 5.0]);
    }

    /// A dash that spans a corner has to bend with it rather than cut it off.
    #[test]
    fn a_dash_that_crosses_a_corner_keeps_the_corner() {
        let runs = dashed(
            &line(&[(0.0, 0.0), (10.0, 0.0), (10.0, 10.0)]),
            &[15.0, 5.0],
        );
        assert_eq!(runs[0].len(), 3, "the corner is still in the dash");
        assert_eq!(runs[0][1], [10.0, 0.0]);
        assert!((length(&runs[0]) - 15.0).abs() < 0.01);
    }

    /// `[3]` means three on, three off -- the same rule SVG has.
    #[test]
    fn an_odd_pattern_repeats_itself() {
        assert_eq!(normalise(&[3.0]), vec![3.0, 3.0]);
        assert_eq!(
            normalise(&[1.0, 2.0, 3.0]),
            vec![1.0, 2.0, 3.0, 1.0, 2.0, 3.0]
        );
        assert_eq!(normalise(&[2.0, 1.0]), vec![2.0, 1.0]);
    }

    #[test]
    fn no_pattern_means_no_dashing() {
        assert!(normalise(&[]).is_empty());
        assert!(normalise(&[0.0, 0.0]).is_empty());
        assert!(dashed(&line(&[(0.0, 0.0), (10.0, 0.0)]), &[]).is_empty());
    }

    /// A zero in the middle of a pattern is legal and must not spin.
    #[test]
    fn survives_a_zero_length_entry() {
        let runs = dashed(&line(&[(0.0, 0.0), (50.0, 0.0)]), &[10.0, 0.0]);
        assert!(!runs.is_empty());
        assert!(runs.len() < 100, "produced {} runs", runs.len());
    }

    #[test]
    fn a_line_shorter_than_the_first_dash_is_drawn_whole() {
        let runs = dashed(&line(&[(0.0, 0.0), (4.0, 0.0)]), &[10.0, 10.0]);
        assert_eq!(runs.len(), 1);
        assert!((length(&runs[0]) - 4.0).abs() < 0.01);
    }
}
