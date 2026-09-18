use std::collections::HashMap;

use sea_orm::{ColumnTrait, Order, QueryOrder};

/// Apply order_by dynamically based on user input.
pub fn apply_sort<C, Q>(
    mut query: Q,
    sort_by: Option<&str>,
    asc: bool,
    columns: &HashMap<&str, C>,
) -> Q
where
    C: ColumnTrait + Copy,
    Q: QueryOrder + Sized,
{
    if let Some(field) = sort_by
        && let Some(column) = columns.get(field)
    {
        let order = if asc { Order::Asc } else { Order::Desc };
        query = query.order_by(*column, order);
    }
    query
}
