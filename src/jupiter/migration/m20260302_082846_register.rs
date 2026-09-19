mod m20260302_082846_add_cla_sign_status;

fn cla_status_create_migration() -> Box<dyn sea_orm_migration::MigrationTrait> {
    Box::new(m20260302_082846_add_cla_sign_status::Migration)
}
