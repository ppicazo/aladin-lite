//! Turning a header we parsed into a WCS.
//!
//! `WCSParams` derives `Deserialize` with `rename_all = "UPPERCASE"`, and our
//! keyword map is already keyed that way, so the sky solution comes straight
//! out of the header with no intermediate representation.

use crate::header::Keywords;
use crate::index::HduEntry;
pub use fitsrs::wcs::{ImgXY, LonLat, WCSParams, WCS};

/// Build a WCS from a parsed header, or `None` if the header does not describe
/// one (or describes one we cannot follow).
pub fn wcs_from_keywords(keywords: &Keywords) -> Option<WCS> {
    let params: WCSParams =
        serde_json::from_value(serde_json::Value::Object(keywords.clone())).ok()?;
    WCS::new(&params).ok()
}

impl HduEntry {
    /// The sky solution for this HDU, if it has one.
    pub fn wcs(&self) -> Option<WCS> {
        wcs_from_keywords(&self.keywords)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn tan_header() -> Keywords {
        let Value::Object(map) = json!({
            "SIMPLE": true,
            "BITPIX": -32,
            "NAXIS": 2,
            "NAXIS1": 4096,
            "NAXIS2": 4096,
            "CTYPE1": "RA---TAN",
            "CTYPE2": "DEC--TAN",
            "CRPIX1": 2048.0,
            "CRPIX2": 2048.0,
            "CRVAL1": 83.822,
            "CRVAL2": -5.391,
            "CDELT1": -0.000488,
            "CDELT2": 0.000488,
        }) else {
            unreachable!()
        };
        map
    }

    #[test]
    fn builds_a_wcs_from_a_tan_header() {
        let wcs = wcs_from_keywords(&tan_header()).expect("TAN header should yield a WCS");
        assert_eq!(wcs.img_dimensions(), [4096, 4096]);

        // The reference pixel must map back to the reference world coordinate.
        let lonlat = wcs
            .unproj_lonlat(&ImgXY::new(2048.0, 2048.0))
            .expect("reference pixel lies on the sky");
        assert!((lonlat.lon().to_degrees() - 83.822).abs() < 1e-3);
        assert!((lonlat.lat().to_degrees() - (-5.391)).abs() < 1e-3);
    }

    #[test]
    fn a_header_without_ctype1_has_no_wcs() {
        let mut keywords = tan_header();
        keywords.remove("CTYPE1");
        assert!(wcs_from_keywords(&keywords).is_none());
    }

    #[test]
    fn unrelated_keywords_do_not_break_deserialisation() {
        let mut keywords = tan_header();
        keywords.insert("ORIGIN".into(), Value::String("gen_fits".into()));
        keywords.insert("BSCALE".into(), Value::Number(1.into()));
        assert!(wcs_from_keywords(&keywords).is_some());
    }
}
