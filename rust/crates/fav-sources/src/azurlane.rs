//! Azur Lane on l2d.su (WP11): the AZP2 envelope of its data files ([`protected`]) and its
//! model sources, catalog and resource checks ([`l2d_sources`]), and the crawl that archives
//! the models and writes the backend manifests ([`crawl`], WP12).

pub mod crawl;
pub mod l2d_sources;
pub mod protected;
