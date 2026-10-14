//! Package and projected feature requests for one Cargo build shape.

use std::collections::BTreeSet;

use super::features::{DroppedToken, FeatureOracle, Projected, WorkspaceFeatures, project};
use crate::error::DevError;
use crate::profile::PlannedSweep;

/// Cargo's package set, without lane eligibility or invocation provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PackageSelection {
    Packages(Vec<String>),
    WorkspaceExcluding(Vec<String>),
    Bare,
}

impl PackageSelection {
    fn packages(&self) -> Option<&[String]> {
        match self {
            Self::Packages(p) => Some(p),
            Self::WorkspaceExcluding(_) | Self::Bare => None,
        }
    }

    fn concrete(&self, ws: &WorkspaceFeatures) -> Vec<String> {
        match self {
            Self::Packages(p) => p.clone(),
            Self::WorkspaceExcluding(excluded) => ws
                .member_names()
                .into_iter()
                .filter(|p| !excluded.contains(p))
                .collect(),
            Self::Bare => ws.default_members().to_vec(),
        }
    }
}

pub(crate) fn package_args(selection: &PackageSelection) -> Vec<String> {
    match selection {
        PackageSelection::Packages(p) => p
            .iter()
            .flat_map(|p| ["-p".to_owned(), p.clone()])
            .collect(),
        PackageSelection::WorkspaceExcluding(excluded) => {
            let mut args = vec!["--workspace".to_owned()];
            for p in excluded {
                args.extend(["--exclude".to_owned(), p.clone()]);
            }
            args
        }
        PackageSelection::Bare => Vec::new(),
    }
}

/// One cargo resolution of an attempted lane: the request one cargo
/// invocation runs with - its package selection and the sweep's features
/// projected onto it. Only this module builds one (the private fields),
/// so every argv carrying a sweep's features carries a projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolutionRun {
    selection: PackageSelection,
    /// The feature argv (`--all-features`, `--no-default-features`,
    /// `--features a,b`) this run hands cargo.
    features: Vec<String>,
    /// The configured tokens this run does not carry, with why.
    dropped: Vec<DroppedToken>,
}

impl ResolutionRun {
    pub(crate) fn new(
        sweep: &PlannedSweep,
        selection: PackageSelection,
        domain: &PackageSelection,
        oracle: &FeatureOracle<'_>,
    ) -> Result<Self, DevError> {
        let (features, dropped) = projected_features(sweep, &selection, domain, &[], oracle)?;
        Ok(Self {
            selection,
            features,
            dropped,
        })
    }

    /// The package-selection flags ([`package_args`]).
    pub(crate) fn package_args(&self) -> Vec<String> {
        package_args(&self.selection)
    }

    /// The projected feature flags.
    pub(crate) fn feature_args(&self) -> &[String] {
        &self.features
    }

    /// Package then feature flags: the request's whole cargo fragment.
    #[cfg(test)]
    pub(crate) fn cargo_args(&self) -> Vec<String> {
        let mut args = self.package_args();
        args.extend(self.features.iter().cloned());
        args
    }

    pub(crate) fn dropped(&self) -> &[DroppedToken] {
        &self.dropped
    }
}

/// One `build_packages` pre-build: the support package alone, with the
/// sweep's features projected onto it. The domain the tokens were written
/// against is the phase's configured selection plus every support package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SupportRequest {
    package: String,
    features: Vec<String>,
    dropped: Vec<DroppedToken>,
}

impl SupportRequest {
    pub(crate) fn new(
        sweep: &PlannedSweep,
        package: String,
        domain: &PackageSelection,
        oracle: &FeatureOracle<'_>,
    ) -> Result<Self, DevError> {
        let own = PackageSelection::Packages(vec![package.clone()]);
        let (features, dropped) =
            projected_features(sweep, &own, domain, &sweep.build_packages, oracle)?;
        Ok(Self {
            package,
            features,
            dropped,
        })
    }

    pub(crate) fn package(&self) -> &str {
        &self.package
    }

    /// `-p <package>`: its own explicit single-package selection, never
    /// the lane's test selection.
    pub(crate) fn package_args(&self) -> Vec<String> {
        package_args(&PackageSelection::Packages(vec![self.package.clone()]))
    }

    pub(crate) fn feature_args(&self) -> &[String] {
        &self.features
    }

    pub(crate) fn dropped(&self) -> &[DroppedToken] {
        &self.dropped
    }
}

/// The sweep's features projected onto `selected`, with `domain` (plus
/// `extra_domain`) the selection the tokens were written against.
///
/// The identity, with no metadata consulted, whenever nothing could drop:
/// no tokens, invocation-explicit features (`brokkr check --features`
/// names exactly what the user asked cargo for), or a selection that is
/// the domain itself. Otherwise both sides are resolved to concrete
/// members through the invocation's one metadata snapshot.
fn projected_features(
    sweep: &PlannedSweep,
    selected: &PackageSelection,
    domain: &PackageSelection,
    extra_domain: &[String],
    oracle: &FeatureOracle<'_>,
) -> Result<(Vec<String>, Vec<DroppedToken>), DevError> {
    let cfg = &sweep.features;
    let identity = (cfg.argv_with(&cfg.tokens), Vec::new());
    if cfg.tokens.is_empty() || cfg.invocation_explicit {
        return Ok(identity);
    }
    if extra_domain
        .iter()
        .all(|x| selected.packages().is_some_and(|p| p.contains(x)))
        && selected == domain
    {
        return Ok(identity);
    }
    let named = |sel: &PackageSelection| {
        sel.packages()
            .map(|p| p.iter().cloned().collect::<BTreeSet<String>>())
    };
    if let (Some(p), Some(mut s)) = (named(selected), named(domain)) {
        s.extend(extra_domain.iter().cloned());
        if p == s {
            return Ok(identity);
        }
    }
    let ws = oracle.get()?;
    let p = selected.concrete(ws);
    let mut s = domain.concrete(ws);
    for x in extra_domain {
        if !s.contains(x) {
            s.push(x.clone());
        }
    }
    let Projected { kept, dropped } = project(&cfg.tokens, &p, &s, ws);
    Ok((cfg.argv_with(&kept), dropped))
}
