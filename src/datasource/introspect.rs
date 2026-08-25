use super::{Column, DataSourceError, Schema, Table, TableKind};

pub(crate) const LIST_SCHEMAS: &str = "
SELECT n.nspname AS name,
       (n.nspname = 'information_schema' OR n.nspname ~ '^pg_') AS is_system
FROM pg_catalog.pg_namespace n
ORDER BY is_system, n.nspname
";

pub(crate) const LIST_TABLES: &str = "
SELECT c.relname AS name, c.relkind::text AS kind
FROM pg_catalog.pg_class c
JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
WHERE n.nspname = $1
  AND c.relkind = ANY (ARRAY['r','p','v','m','f'])
ORDER BY c.relname
";

pub(crate) const LIST_COLUMNS: &str = "
SELECT a.attname                                   AS name,
       pg_catalog.format_type(a.atttypid, a.atttypmod) AS data_type,
       NOT a.attnotnull                            AS is_nullable,
       pg_catalog.pg_get_expr(d.adbin, d.adrelid)  AS default_expr,
       a.attnum                                    AS ordinal,
       COALESCE(pk.is_pk, false)                   AS is_primary_key
FROM pg_catalog.pg_attribute a
JOIN pg_catalog.pg_class c     ON c.oid = a.attrelid
JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
LEFT JOIN pg_catalog.pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum
LEFT JOIN LATERAL (
    SELECT true AS is_pk
    FROM pg_catalog.pg_index i
    WHERE i.indrelid = c.oid AND i.indisprimary AND a.attnum = ANY (i.indkey)
) pk ON true
WHERE n.nspname = $1 AND c.relname = $2
  AND a.attnum > 0 AND NOT a.attisdropped
ORDER BY a.attnum
";

pub(crate) fn row_to_schema(row: &tokio_postgres::Row) -> Schema {
    Schema {
        name: row.get("name"),
        is_system: row.get("is_system"),
    }
}

pub(crate) fn row_to_table(row: &tokio_postgres::Row) -> Result<Table, DataSourceError> {
    let name: String = row.get("name");
    let kind: String = row.get("kind");
    let kind = relkind_from_str(&kind)?;
    Ok(Table { name, kind })
}

pub(crate) fn row_to_column(row: &tokio_postgres::Row) -> Column {
    Column {
        name: row.get("name"),
        data_type: row.get("data_type"),
        is_nullable: row.get("is_nullable"),
        default_expr: row.get("default_expr"),
        ordinal: row.get("ordinal"),
        is_primary_key: row.get("is_primary_key"),
    }
}

fn relkind_from_str(kind: &str) -> Result<TableKind, DataSourceError> {
    match kind {
        "r" => Ok(TableKind::Table),
        "p" => Ok(TableKind::PartitionedTable),
        "v" => Ok(TableKind::View),
        "m" => Ok(TableKind::MaterializedView),
        "f" => Ok(TableKind::ForeignTable),
        other => Err(DataSourceError::UnexpectedRelkind {
            kind: other.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relkind_maps_known_chars() {
        assert_eq!(relkind_from_str("r").unwrap(), TableKind::Table);
        assert_eq!(relkind_from_str("p").unwrap(), TableKind::PartitionedTable);
        assert_eq!(relkind_from_str("v").unwrap(), TableKind::View);
        assert_eq!(relkind_from_str("m").unwrap(), TableKind::MaterializedView);
        assert_eq!(relkind_from_str("f").unwrap(), TableKind::ForeignTable);
    }

    #[test]
    fn relkind_rejects_unknown_char() {
        let err = relkind_from_str("i").unwrap_err();
        assert!(matches!(
            err,
            DataSourceError::UnexpectedRelkind { kind } if kind == "i"
        ));
    }
}
