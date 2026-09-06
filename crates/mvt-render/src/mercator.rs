//! Web Mercator helpers.
//!
//! World coordinates are normalized to [0, 1]: north is small y, the
//! antimeridian is x = 0 / x = 1. Ported from the earlier `tile_renderer`
//! prototype's `projection.h`, which used pixel units instead.

const DEG2RAD: f64 = std::f64::consts::PI / 180.0;

/// Longitude in degrees -> normalized world x.
pub fn merc_x(lon: f64) -> f64 {
    (lon + 180.0) / 360.0
}

/// Latitude in degrees -> normalized world y.
pub fn merc_y(lat: f64) -> f64 {
    let clamped = lat.clamp(-89.999_999, 89.999_999);
    0.5 - (std::f64::consts::PI / 4.0 + clamped * DEG2RAD / 2.0)
        .tan()
        .ln()
        / (2.0 * std::f64::consts::PI)
}

/// Normalized world x -> longitude in degrees.
pub fn inv_merc_x(x: f64) -> f64 {
    x * 360.0 - 180.0
}

/// Normalized world y -> latitude in degrees.
pub fn inv_merc_y(y: f64) -> f64 {
    (2.0 * ((0.5 - y) * 2.0 * std::f64::consts::PI).exp().atan() - std::f64::consts::PI / 2.0)
        / DEG2RAD
}

/// Center of tile (z, x, y) as (lon, lat).
pub fn tile_center(z: u8, x: u32, y: u32) -> (f64, f64) {
    let n = (1u64 << z) as f64;
    (
        inv_merc_x((x as f64 + 0.5) / n),
        inv_merc_y((y as f64 + 0.5) / n),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_mercator() {
        for &(lon, lat) in &[
            (0.0, 0.0),
            (139.767, 35.681),
            (-122.4, 37.8),
            (180.0, -60.0),
        ] {
            let rx = inv_merc_x(merc_x(lon));
            let ry = inv_merc_y(merc_y(lat));
            assert!((rx - lon).abs() < 1e-9, "lon {lon} -> {rx}");
            assert!((ry - lat).abs() < 1e-9, "lat {lat} -> {ry}");
        }
    }

    #[test]
    fn tile_zero_is_the_whole_world() {
        let (lon, lat) = tile_center(0, 0, 0);
        assert!(lon.abs() < 1e-9);
        assert!(lat.abs() < 1e-9);
    }

    #[test]
    fn tile_center_matches_quadrant() {
        // z=1 x=0 y=0 is the north-west quadrant.
        let (lon, lat) = tile_center(1, 0, 0);
        assert!(lon < 0.0 && lat > 0.0);
    }
}
