pub mod index;
pub mod cache;

pub use index::{
    Collection, FeatureMetadata, TemplateMetadata, OptionDef,
    fetch_collection_index, fetch_templates, fetch_features,
};
