-- One line per schema object in the current schema, in a stable order. fav-db's schema
-- test runs it against a database `ensure_schema` created and compares with snapshot.txt
-- (which tools/schema/dump_schema.py produced with this same text from the Python app,
-- tag `python-final`).
-- Column positions are counted over live columns, so a column Python added and later
-- dropped leaves no gap.
SELECT line FROM (
    SELECT c.relname AS tbl, 0 AS kind, '' AS name, 0 AS pos,
           'table ' || c.relname || CASE WHEN c.relpersistence = 'u' THEN ' unlogged' ELSE '' END AS line
    FROM pg_class c
    WHERE c.relnamespace = current_schema()::regnamespace AND c.relkind IN ('r', 'p')
    UNION ALL
    SELECT c.relname, 1, a.attname,
           row_number() OVER (PARTITION BY c.oid ORDER BY a.attnum)::int,
           'column ' || c.relname || '.' || row_number() OVER (PARTITION BY c.oid ORDER BY a.attnum)
               || ' ' || a.attname || ' ' || format_type(a.atttypid, a.atttypmod)
               || CASE WHEN a.attnotnull THEN ' not null' ELSE '' END
               || CASE WHEN a.attidentity <> '' THEN ' identity ' || a.attidentity::text ELSE '' END
               || COALESCE(' default ' || pg_get_expr(d.adbin, d.adrelid), '')
    FROM pg_class c
    JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped
    LEFT JOIN pg_attrdef d ON d.adrelid = c.oid AND d.adnum = a.attnum
    WHERE c.relnamespace = current_schema()::regnamespace AND c.relkind IN ('r', 'p')
    UNION ALL
    SELECT c.relname, 2, con.conname, 0,
           'constraint ' || c.relname || ' ' || con.conname || ' ' || pg_get_constraintdef(con.oid)
    FROM pg_constraint con
    JOIN pg_class c ON c.oid = con.conrelid
    WHERE c.relnamespace = current_schema()::regnamespace
    UNION ALL
    SELECT c.relname, 3, i.relname, 0,
           'index ' || c.relname || ' ' || i.relname || ' ' || pg_get_indexdef(i.oid)
    FROM pg_index x
    JOIN pg_class c ON c.oid = x.indrelid
    JOIN pg_class i ON i.oid = x.indexrelid
    WHERE c.relnamespace = current_schema()::regnamespace
    UNION ALL
    SELECT s.relname, 4, s.relname, 0,
           'sequence ' || s.relname || ' ' || format_type(q.seqtypid, NULL)
               || ' start ' || q.seqstart || ' increment ' || q.seqincrement
               || ' min ' || q.seqmin || ' max ' || q.seqmax || ' cache ' || q.seqcache
               || CASE WHEN q.seqcycle THEN ' cycle' ELSE '' END
               || COALESCE(' owned by ' || owner.relname || '.' || owner_column.attname, '')
    FROM pg_sequence q
    JOIN pg_class s ON s.oid = q.seqrelid
    LEFT JOIN pg_depend dep
      ON dep.classid = 'pg_class'::regclass AND dep.objid = s.oid
     AND dep.refclassid = 'pg_class'::regclass AND dep.deptype IN ('a', 'i')
    LEFT JOIN pg_class owner ON owner.oid = dep.refobjid
    LEFT JOIN pg_attribute owner_column ON owner_column.attrelid = dep.refobjid AND owner_column.attnum = dep.refobjsubid
    WHERE s.relnamespace = current_schema()::regnamespace
) AS objects
ORDER BY tbl, kind, pos, name, line;
