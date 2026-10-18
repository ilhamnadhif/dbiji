//! Labels that are translated at run time (`tr(kind.label())`) must have
//! Indonesian entries too; the literal-string test cannot see them.

use dbiji_core::meta::ObjectKind;
use std::collections::BTreeSet;

#[test]
fn runtime_labels_are_translated() {
    let have: BTreeSet<&str> = dbiji_core::i18n::translated_keys().into_iter().collect();
    let mut labels: Vec<&'static str> = Vec::new();
    for a in dbiji_config::Action::ALL {
        labels.push(a.label());
        labels.push(a.category().label());
    }
    use ObjectKind::*;
    for k in [
        Database,
        Schema,
        Table,
        View,
        MaterializedView,
        ForeignTable,
        Column,
        Index,
        PrimaryKey,
        UniqueKey,
        CheckConstraint,
        ForeignKey,
        Trigger,
        Sequence,
        Function,
        Procedure,
        Aggregate,
        Type,
        Domain,
        Extension,
        Event,
        Partition,
        Synonym,
        Role,
        Tablespace,
    ] {
        labels.push(k.label());
        labels.push(k.plural_label());
    }
    use dbiji_sql::StatementKind as S;
    for k in [
        S::Select,
        S::Insert,
        S::Update,
        S::Delete,
        S::Merge,
        S::Create,
        S::Alter,
        S::Drop,
        S::Truncate,
        S::Rename,
        S::Comment,
        S::Grant,
        S::Revoke,
        S::Set,
        S::Show,
        S::Use,
        S::Call,
        S::Explain,
        S::Describe,
        S::Copy,
        S::Maintenance,
        S::Pragma,
        S::Lock,
        S::ClientCommand,
        S::Other,
        S::Transaction(dbiji_sql::TxCommand::Begin),
    ] {
        labels.push(k.label());
    }
    use dbiji_sql::Danger as D;
    for d in [D::DeleteWithoutWhere, D::UpdateWithoutWhere, D::Drop, D::Truncate, D::AlterDrop] {
        labels.push(d.message());
    }
    use dbiji_compare::DiffStatus as C;
    for s in [C::Added, C::Removed, C::Modified, C::Unchanged] {
        labels.push(s.label());
    }
    use dbiji_tasks::RunStatus as R;
    for s in [R::Running, R::Succeeded, R::Failed, R::Cancelled, R::Skipped] {
        labels.push(s.label());
    }
    let mut missing: Vec<&str> = labels.into_iter().filter(|l| !have.contains(l)).collect();
    missing.sort();
    missing.dedup();
    if !missing.is_empty() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/i18n-missing-labels.txt");
        let _ = std::fs::write(&root, missing.join("\n"));
    }
    assert!(missing.is_empty(), "{} runtime labels without Indonesian translation (see target/i18n-missing-labels.txt): {missing:?}", missing.len());
}
