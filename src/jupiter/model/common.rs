use crate::callisto::mega_cl;

/// CL-only item wrapper after CE-22 Issue HTTP retirement (CE-23 upstream table drop).
pub enum ItemKind {
    Cl(mega_cl::Model),
}

pub struct ItemDetails {
    pub item: ItemKind,
    pub comment_num: usize,
}

pub struct ListParams {
    pub status: String,
    pub author: Option<String>,
    pub sort_by: Option<String>,
    pub asc: bool,
}
