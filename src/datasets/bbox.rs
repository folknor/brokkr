//! Parsed bounding boxes and geometry conversions.

use std::fmt;

/// Four finite bbox coordinates in `lon_min,lat_min,lon_max,lat_max` order.
///
/// The original spelling is retained for pbfhogg argv and result-row
/// compatibility; geometry and API conversion use the parsed finite values.
/// Parsing enforces arity and finiteness while leaving coordinate-range and
/// min/max ordering policy unchanged.
#[derive(Debug, Clone, PartialEq)]
pub struct Bbox {
    coordinates: [f64; 4],
    source: String,
}

impl Bbox {
    /// Parse four finite comma-separated coordinates while retaining their
    /// source spelling for command-line reporting.
    pub fn parse(value: &str) -> Result<Self, crate::error::DevError> {
        let parts: Vec<&str> = value.split(',').collect();
        if parts.len() != 4 {
            return Err(crate::error::DevError::Config(format!(
                "bbox must have 4 comma-separated values, got {}: '{value}'",
                parts.len()
            )));
        }

        let mut coordinates = [0.0; 4];
        for (index, part) in parts.iter().enumerate() {
            let trimmed = part.trim();
            let coordinate = trimmed.parse::<f64>().map_err(|_| {
                crate::error::DevError::Config(format!(
                    "bbox component {index} is not a number: '{trimmed}'"
                ))
            })?;
            if !coordinate.is_finite() {
                return Err(crate::error::DevError::Config(format!(
                    "bbox component {index} is not finite: '{trimmed}'"
                )));
            }
            coordinates[index] = coordinate;
        }

        Ok(Self {
            coordinates,
            source: value.to_owned(),
        })
    }

    /// Original bbox spelling, retained for pbfhogg argv and diagnostics.
    pub fn as_str(&self) -> &str {
        &self.source
    }

    /// pbfhogg's single-token bbox option. Keeping the `=` is required when
    /// the west longitude begins with `-`.
    pub fn to_pbfhogg_arg(&self) -> String {
        format!("-b={}", self.source)
    }

    /// Coordinates in brokkr order: longitude minimum, latitude minimum,
    /// longitude maximum, latitude maximum.
    pub fn coordinates(&self) -> [f64; 4] {
        self.coordinates
    }

    fn from_coordinates(coordinates: [f64; 4]) -> Result<Self, crate::error::DevError> {
        if coordinates.iter().any(|coordinate| !coordinate.is_finite()) {
            return Err(crate::error::DevError::Config(
                "bbox geometry produced a non-finite coordinate".into(),
            ));
        }
        Ok(Self {
            source: format!(
                "{},{},{},{}",
                coordinates[0], coordinates[1], coordinates[2], coordinates[3]
            ),
            coordinates,
        })
    }

    /// Convert to the nidhogg API's `[lat_min,lon_min,lat_max,lon_max]` order.
    pub fn to_nidhogg_api(&self) -> String {
        let [lon_min, lat_min, lon_max, lat_max] = self.coordinates;
        format!("[{lat_min},{lon_min},{lat_max},{lon_max}]")
    }

    /// Split into equal-width longitude strips for multi-extract.
    #[allow(clippy::cast_precision_loss)]
    pub fn strips(&self, regions: usize) -> Result<Vec<Self>, crate::error::DevError> {
        if regions == 0 {
            return Err(crate::error::DevError::Config(
                "multi-extract requires at least 1 region".into(),
            ));
        }
        let [min_lon, min_lat, max_lon, max_lat] = self.coordinates;
        let strip_width = (max_lon - min_lon) / regions as f64;
        (0..regions)
            .map(|index| {
                let strip_min = min_lon + strip_width * index as f64;
                let strip_max = if index + 1 == regions {
                    max_lon
                } else {
                    min_lon + strip_width * (index + 1) as f64
                };
                Self::from_coordinates([strip_min, min_lat, strip_max, max_lat])
            })
            .collect()
    }

    /// Shrink to the center quarter, preserving the existing coordinate
    /// calculation used by nidhogg's smaller-area queries.
    pub fn shrink_quarter(&self) -> Result<Self, crate::error::DevError> {
        let [lon_min, lat_min, lon_max, lat_max] = self.coordinates;
        let lon_mid = (lon_min + lon_max) / 2.0;
        let lat_mid = (lat_min + lat_max) / 2.0;
        let lon_q = (lon_max - lon_min) / 4.0;
        let lat_q = (lat_max - lat_min) / 4.0;
        let coordinates = [
            lon_mid - lon_q,
            lat_mid - lat_q,
            lon_mid + lon_q,
            lat_mid + lat_q,
        ];
        Self::from_coordinates(coordinates)
    }
}

impl fmt::Display for Bbox {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.source)
    }
}
