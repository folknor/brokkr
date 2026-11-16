//! Parsed bounding boxes and geometry conversions.

use std::fmt;

/// Four finite bbox coordinates in `lon_min,lat_min,lon_max,lat_max` order.
///
/// The original spelling is retained for config reporting; geometry and API
/// conversion use the parsed finite values.
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

    /// Coordinates in brokkr order: longitude minimum, latitude minimum,
    /// longitude maximum, latitude maximum.
    pub fn coordinates(&self) -> [f64; 4] {
        self.coordinates
    }
}

impl fmt::Display for Bbox {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.source)
    }
}
