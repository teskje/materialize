# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

$ set-arg-default default-replica-size=1
$ set-arg-default single-replica-cluster=quickstart

$ set-regex match=u\d+|cluster1|quickstart|testdrive_single_replica_cluster|testdrive-data-\d+ replacement=<VARIABLE_OUTPUT>

$ set writer-schema={
    "name": "row",
    "type": "record",
    "fields": [
      {"name": "a", "type": "long"},
      {"name": "b", "type": "string"}
    ]
  }

$ kafka-create-topic topic=data
$ kafka-ingest topic=data format=avro schema=${writer-schema}
{"a": 1, "b": "dog"}

# Create library of objects and verify names
> CREATE CONNECTION kafka_conn
  TO KAFKA (BROKER '${testdrive.kafka-addr}', SECURITY PROTOCOL PLAINTEXT);

> CREATE CONNECTION IF NOT EXISTS csr_conn TO CONFLUENT SCHEMA REGISTRY (
    URL '${testdrive.schema-registry-url}'
  );

> CREATE SOURCE mz_data
  IN CLUSTER ${arg.single-replica-cluster}
  FROM KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-data-${testdrive.seed}')

> CREATE TABLE mz_data_tbl FROM SOURCE mz_data (REFERENCE "testdrive-data-${testdrive.seed}")
  FORMAT AVRO USING SCHEMA '${writer-schema}'

> CREATE DEFAULT INDEX ON mz_data_tbl

> CREATE SINK sink1
  IN CLUSTER ${arg.single-replica-cluster}
  FROM mz_data_tbl
  INTO KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-snk1-${testdrive.seed}')
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY CONNECTION csr_conn
  ENVELOPE DEBEZIUM

> CREATE VIEW mz_view AS
    SELECT * FROM mz_data_tbl

> CREATE DEFAULT INDEX ON mz_view

> SHOW INDEXES ON mz_view
name                on          cluster             key    comment
--------------------------------------------------------------------------------------------
mz_view_primary_idx mz_view     <VARIABLE_OUTPUT>   {a,b}  ""

> CREATE VIEW dependent_view AS
    SELECT * FROM mz_view;

> CREATE VIEW byzantine_view AS
    SELECT mz_data_tbl.a, mz_view.b FROM mz_data_tbl JOIN mz_view ON mz_data_tbl.a = mz_view.a;

> CREATE VIEW oppositional_view AS
    SELECT * FROM mz_view WHERE b = '
    an adversarial string
    "materialize"."public"."mz_data_tbl"
    ';

> CREATE VIEW public_objects AS
    SELECT mz_objects.id, mz_objects.oid, mz_objects.schema_id, mz_objects.name, mz_objects.type
    FROM mz_objects
    JOIN mz_schemas ON mz_objects.schema_id = mz_schemas.id
    WHERE mz_schemas.name = 'public';

> SELECT name FROM public_objects WHERE name LIKE 'mz_data%';
name
-----
mz_data
mz_data_tbl
mz_data_tbl_primary_idx
mz_data_progress

# Test that data can be selected from the source before renaming.
> SELECT * FROM mz_data_tbl
a  b
------
1  dog

! ALTER VIEW mz_data RENAME TO renamed_mz_data;
exact:mz_data is a source not a view

! ALTER INDEX mz_data RENAME TO renamed_mz_data;
exact:mz_data is a source not an index

> ALTER SOURCE mz_data RENAME TO renamed_mz_data;

> SELECT name FROM public_objects WHERE name LIKE 'mz_data%';
name
-----
mz_data_tbl_primary_idx
mz_data_tbl
mz_data_progress

> ALTER TABLE mz_data_tbl RENAME TO renamed_mz_data_tbl;

> SELECT name FROM public_objects WHERE name LIKE 'mz_data%';
name
-----
mz_data_tbl_primary_idx
mz_data_progress

> SELECT name FROM public_objects WHERE name LIKE 'renamed_mz_data%';
name
-----
renamed_mz_data
renamed_mz_data_tbl

# Test that data can be selected from the source after renaming.
> SELECT * FROM renamed_mz_data_tbl
a  b
------
1  dog

# Test that data can be selected from the source if it is rematerialized with
# the new name. This previously tripped an assertion that asserted that a source
# descriptor never changed; it is in fact okay for the name of a source to
# change.
> DROP INDEX mz_data_tbl_primary_idx
> CREATE DEFAULT INDEX ON renamed_mz_data_tbl
> SELECT * FROM renamed_mz_data_tbl
a  b
------
1  dog

> SELECT name FROM public_objects WHERE name LIKE 'mz_view%';
name
-----
mz_view
mz_view_primary_idx

! ALTER SOURCE mz_view RENAME TO renamed_mz_view;
exact:mz_view is a view not a source

! ALTER INDEX mz_view RENAME TO renamed_mz_view;
exact:mz_view is a view not an index

> ALTER VIEW mz_view RENAME TO renamed_mz_view;

> SELECT name FROM public_objects WHERE name LIKE 'mz_view%';
name
-----
mz_view_primary_idx

> SELECT name FROM public_objects WHERE name LIKE 'renamed_mz_view%';
name
-----
renamed_mz_view

> SELECT name FROM public_objects WHERE name LIKE 'mz_view_primary_idx%';
name
-----
mz_view_primary_idx

! ALTER VIEW mz_view_primary_idx RENAME TO renamed_index;
exact:mz_view_primary_idx is an index not a view

! ALTER SOURCE mz_view_primary_idx RENAME TO renamed_index;
exact:mz_view_primary_idx is an index not a source

> ALTER INDEX mz_view_primary_idx RENAME TO renamed_index;

> SELECT name FROM public_objects WHERE name LIKE 'mz_view_primary_idx%';
name
-----

> SELECT name FROM public_objects WHERE name LIKE 'renamed_index%';
name
-----
renamed_index

> SELECT name FROM public_objects WHERE name LIKE 'sink1%';
name
-----
sink1

! ALTER VIEW sink1 RENAME TO renamed_sink;
exact:sink1 is a sink not a view

> ALTER SINK sink1 RENAME TO renamed_sink

> SELECT name FROM public_objects WHERE name LIKE 'sink1%';
name
-----

> SELECT name FROM public_objects WHERE name LIKE 'renamed_sink%';
name
-----
renamed_sink

# Clean up temp view
> DROP VIEW public_objects;

# Source was successfully renamed
> SHOW SOURCES;
name               type      cluster            comment
-----------------------------------------------------------------
mz_data_progress   progress  <null>             ""
renamed_mz_data    kafka     <VARIABLE_OUTPUT>  ""

# Sink was successfully renamed
> SHOW SINKS
name               type   cluster            comment
----------------------------------------------------
renamed_sink       kafka  <VARIABLE_OUTPUT>  ""

# View was successfully renamed
> SHOW VIEWS
name               comment
--------------------------
byzantine_view     ""
dependent_view     ""
oppositional_view  ""
renamed_mz_view    ""

# Item's own `CREATE VIEW` statement updated
> SHOW CREATE VIEW renamed_mz_view
name                                create_sql
---------------------------------------------------------------------------------------------------------------------------------------------------
materialize.public.renamed_mz_view "CREATE VIEW\n    materialize.public.renamed_mz_view AS SELECT * FROM materialize.public.renamed_mz_data_tbl;"

# Item's indexes are properly re-attributed
> SHOW INDEXES ON renamed_mz_view
name            on              cluster             key    comment
--------------------------------------------------------------------------------------------
renamed_index   renamed_mz_view <VARIABLE_OUTPUT>   {a,b}  ""

> SHOW CREATE INDEX renamed_index
name                             create_sql
---------------------------------------------------------------------------------------------------------------------
materialize.public.renamed_index "CREATE INDEX renamed_index IN CLUSTER <VARIABLE_OUTPUT> ON materialize.public.renamed_mz_view (a, b);"

# Simple dependencies are renamed
> SHOW CREATE VIEW dependent_view
name                                create_sql
------------------------------------------------------------------------------------------------------------------------------------------------
materialize.public.dependent_view "CREATE VIEW materialize.public.dependent_view AS SELECT * FROM materialize.public.renamed_mz_view;"

> SHOW CREATE SINK renamed_sink
name                            create_sql
-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------
materialize.public.renamed_sink "CREATE SINK materialize.public.renamed_sink IN CLUSTER <VARIABLE_OUTPUT> FROM materialize.public.renamed_mz_data_tbl INTO KAFKA CONNECTION materialize.public.kafka_conn (TOPIC = 'testdrive-snk1-${testdrive.seed}') FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY CONNECTION materialize.public.csr_conn ENVELOPE DEBEZIUM;"

# Simple dependencies with both fully qualified and unqualified item references are renamed
> SHOW CREATE VIEW byzantine_view
name                              create_sql
---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------
materialize.public.byzantine_view "CREATE VIEW\n    materialize.public.byzantine_view\n    AS\n        SELECT renamed_mz_data_tbl.a, renamed_mz_view.b\n        FROM\n            materialize.public.renamed_mz_data_tbl\n                JOIN materialize.public.renamed_mz_view ON renamed_mz_data_tbl.a = renamed_mz_view.a;"

# Strings containing old item name are not modified
> SHOW CREATE VIEW oppositional_view
name                                 create_sql
------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------
materialize.public.oppositional_view "CREATE VIEW\n    materialize.public.oppositional_view\n    AS\n        SELECT *\n        FROM materialize.public.renamed_mz_view\n        WHERE b = '\n  an adversarial string\n  \"materialize\".\"public\".\"mz_data_tbl\"\n  ';"

# 🔬 Name collisions

> ALTER VIEW renamed_mz_view RENAME TO t1
> CREATE VIEW a AS SELECT 1 AS a
> CREATE VIEW v0 AS SELECT 2 AS b
> CREATE VIEW t2 (a, b, t1_a, t1_b) AS
  SELECT * FROM a
  JOIN v0
  ON a.a = v0.b
  JOIN t1
  ON a.a = t1.a;

# ❌ Identifiers used in dependent items
! ALTER VIEW t1 RENAME TO b;
contains:renaming conflict
! ALTER VIEW t1 RENAME TO materialize;
contains:renaming conflict
! ALTER VIEW t1 RENAME TO public;
contains:renaming conflict

# ❌ Identifiers used in own definition
# `materialize.public.a` contains an unqualified reference to `materialize.public.t1.a`.
! ALTER VIEW a RENAME TO anything
contains:renaming conflict
! ALTER VIEW v0 RENAME TO b
contains:renaming conflict

# ❌ Name used by another item in schema's catalog
! ALTER VIEW t1 RENAME TO a
contains:catalog item 'a' already exists
! ALTER VIEW t1 RENAME TO dependent_view;
contains:catalog item 'dependent_view' already exists

# 🔬 Aliases

> CREATE VIEW table_alias_check AS
  SELECT t1.a AS b
  FROM (
    SELECT a
    FROM t1
  ) AS t1;

# ❌ View name used as alias
! ALTER VIEW t1 RENAME TO anything
contains:renaming conflict

# 🔬 Unresolvable without scope analysis
# These are example queries whose ambiguity cannot be resolved through
# simple ident analysis; i.e. you need either real or strictly inferred scope
# information to disambiguate references to `db0.scm0.z`.

> CREATE DATABASE IF NOT EXISTS db0;
> CREATE SCHEMA IF NOT EXISTS db0.scm0;
> CREATE VIEW db0.scm0.z AS SELECT 1 AS a;

> CREATE SCHEMA IF NOT EXISTS materialize.db0;
> CREATE VIEW materialize.db0.scm0 AS SELECT 1 AS z;

> CREATE VIEW final_boss_1 AS
  SELECT db0.scm0.z, db0.scm0.z.a
  FROM db0.scm0.z
  JOIN db0.scm0
  ON db0.scm0.z = db0.scm0.z.a;

# ❌ Item name used as column
! ALTER VIEW db0.scm0.z RENAME TO anything
contains:renaming conflict
# ❌ Item name used as schema
! ALTER VIEW db0.scm0 RENAME TO anything
contains:renaming conflict

> CREATE SCHEMA IF NOT EXISTS materialize.scm0;
> CREATE VIEW materialize.scm0.z AS SELECT 1 AS a;
> CREATE VIEW materialize.public.scm0 AS SELECT 1 AS z;

> CREATE VIEW final_boss_2 AS
  SELECT scm0.z, scm0.z.a
  FROM scm0.z
  JOIN scm0
  ON scm0.z = scm0.z.a;

# ❌ Item name used as column
! ALTER VIEW scm0.z RENAME TO anything
contains:renaming conflict
# ❌ Item name used as schema
! ALTER VIEW scm0 RENAME TO anything
contains:renaming conflict

# 🔬 DB::DB

# 🔬🔬 DB::DB - DB-level qualification
> CREATE DATABASE IF NOT EXISTS db1
> CREATE DATABASE IF NOT EXISTS db2

> CREATE SCHEMA IF NOT EXISTS db1.scm1
> CREATE SCHEMA IF NOT EXISTS db2.scm1

> CREATE VIEW db1.scm1.v AS SELECT 1 as a;
> CREATE VIEW db2.scm1.v AS SELECT 2 as b;

# References to `v` are ambiguous because not all references are sufficiently
# qualified. This won't work until we have database-level qualification.
> CREATE VIEW db1_db2_scm1_min_qual_invalid AS
    SELECT * FROM
    (
        SELECT v.a FROM db1.scm1.v
    ) AS l
    JOIN (
        SELECT v.b FROM db2.scm1.v
    ) AS r
    ON l.a = r.b;

# ❌ Insufficient qualification
! ALTER VIEW db1.scm1.v rename to anything;
contains:renaming conflict
! ALTER VIEW db2.scm1.v rename to anything;
contains:renaming conflict

> DROP VIEW db1_db2_scm1_min_qual_invalid

# References to `v` are ambiguous because not all references are sufficiently
# qualified.
> CREATE VIEW db1_db2_scm1_mix_qual_invalid AS
    SELECT * FROM
    (
        SELECT db1.scm1.v.a FROM db1.scm1.v
    ) AS l
    JOIN (
        SELECT scm1.v.b FROM db2.scm1.v
    ) AS r
    ON l.a = r.b;

# ❌ Insufficient qualification
! ALTER VIEW db1.scm1.v rename to anything;
contains:renaming conflict
! ALTER VIEW db2.scm1.v rename to anything;
contains:renaming conflict

> DROP VIEW db1_db2_scm1_mix_qual_invalid

# Fully qualifying references allows renaming
> CREATE VIEW db1_db2_scm1_valid_qual AS
    SELECT * FROM
    (
        SELECT db1.scm1.v.a FROM db1.scm1.v
    ) AS l
    JOIN (
        SELECT db2.scm1.v.b FROM db2.scm1.v
    ) AS r
    ON l.a = r.b;

# ✅ Sufficient qualification + new idents
> ALTER VIEW db1.scm1.v RENAME TO v0
> ALTER VIEW db1.scm1.v0 RENAME TO v1
> ALTER VIEW db2.scm1.v RENAME TO v2

> SHOW CREATE VIEW db1_db2_scm1_valid_qual
name                                       create_sql
-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------
materialize.public.db1_db2_scm1_valid_qual "CREATE VIEW\n    materialize.public.db1_db2_scm1_valid_qual\n    AS\n        SELECT *\n        FROM\n            (SELECT db1.scm1.v1.a FROM db1.scm1.v1) AS l\n                JOIN (SELECT db2.scm1.v2.b FROM db2.scm1.v2) AS r ON l.a = r.b;"

> CREATE VIEW "materialize"."public"."db1_db2_scm1_valid_qual_test" AS SELECT * FROM (SELECT "db1"."scm1"."v1"."a" FROM "db1"."scm1"."v1") AS "l" JOIN (SELECT "db2"."scm1"."v2"."b" FROM "db2"."scm1"."v2") AS "r" ON "l"."a" = "r"."b"

# 🔬🔬 DB::DB - No qualification necessary
# Evaluate ability to handle two database-qualified references.
> CREATE SCHEMA db2.scm2;
> CREATE VIEW db2.scm2.v2 AS
    SELECT 1 AS z;

> CREATE VIEW db_db_qual_diff_s_v AS
    SELECT db2.scm2.v2.z
    FROM db2.scm2.v2
    JOIN db1.scm1.v1
    ON db2.scm2.v2.z = db1.scm1.v1.a

# ❌ Identifiers used in dependent items
! ALTER VIEW db2.scm2.v2 RENAME TO db2;
contains:renaming conflict
! ALTER VIEW db2.scm2.v2 RENAME TO scm2;
contains:renaming conflict
! ALTER VIEW db2.scm2.v2 RENAME TO z;
contains:renaming conflict
! ALTER VIEW db2.scm2.v2 RENAME TO scm2;
contains:renaming conflict
! ALTER VIEW db2.scm2.v2 RENAME TO a;
contains:renaming conflict

# ✅ New idents
> ALTER VIEW db2.scm2.v2 RENAME TO v3;
> ALTER VIEW db1.scm1.v1 RENAME TO v4;

> SHOW CREATE VIEW db_db_qual_diff_s_v
name                                   create_sql
-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------
materialize.public.db_db_qual_diff_s_v "CREATE VIEW\n    materialize.public.db_db_qual_diff_s_v\n    AS SELECT db2.scm2.v3.z FROM db2.scm2.v3 JOIN db1.scm1.v4 ON db2.scm2.v3.z = db1.scm1.v4.a;"

> CREATE VIEW "materialize"."public"."db_db_qual_diff_s_v_test" AS SELECT "db2"."scm2"."v3"."z" FROM "db2"."scm2"."v3" JOIN "db1"."scm1"."v4" ON "db2"."scm2"."v3"."z" = "db1"."scm1"."v4"."a"

# 🔬 DB::Schema
# Evaluate ability to handle one database-qualified reference and one
# schema-qualified reference.

> CREATE SCHEMA IF NOT EXISTS scm3

> CREATE VIEW scm3.v2 AS
    SELECT 1 AS z;

> CREATE VIEW db_scm_qual AS
    SELECT scm3.v2.z, db1.scm1.v4.a
    FROM scm3.v2
    JOIN db1.scm1.v4
    ON scm3.v2.z = db1.scm1.v4.a;

> ALTER VIEW scm3.v2 RENAME TO v3;
> ALTER VIEW db1.scm1.v4 RENAME TO v5;

> SHOW CREATE VIEW db_scm_qual
name                           create_sql
-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------
materialize.public.db_scm_qual "CREATE VIEW\n    materialize.public.db_scm_qual\n    AS\n        SELECT scm3.v3.z, db1.scm1.v5.a\n        FROM materialize.scm3.v3 JOIN db1.scm1.v5 ON scm3.v3.z = db1.scm1.v5.a;"

> CREATE VIEW "materialize"."public"."db_scm_qual_test" AS SELECT "scm3"."v3"."z", "db1"."scm1"."v5"."a" FROM "materialize"."scm3"."v3" JOIN "db1"."scm1"."v5" ON "scm3"."v3"."z" = "db1"."scm1"."v5"."a"

# 🔬 DB::View
# Evaluate ability to handle one database-qualified reference and one
# implicitly-qualified reference.

> CREATE VIEW v1 AS
    SELECT 1 AS z;

> CREATE VIEW db_v_qual AS
    SELECT v1.z, db1.scm1.v5.a
    FROM v1
    JOIN db1.scm1.v5
    ON v1.z = db1.scm1.v5.a;

# ❌ Identifiers used in dependent items
! ALTER VIEW v1 RENAME TO z;
contains:renaming conflict
! ALTER VIEW v1 RENAME TO db1;
contains:renaming conflict
! ALTER VIEW v1 RENAME TO scm1;
contains:renaming conflict
! ALTER VIEW v1 RENAME TO v5;
contains:renaming conflict
! ALTER VIEW db1.scm1.v5 RENAME TO v1;
contains:renaming conflict

# ✅ New idents
> ALTER VIEW v1 RENAME TO v2;
> ALTER VIEW db1.scm1.v5 RENAME TO v6;

> SHOW CREATE VIEW db_v_qual
name                         create_sql
-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------
materialize.public.db_v_qual "CREATE VIEW\n    materialize.public.db_v_qual\n    AS\n        SELECT v2.z, db1.scm1.v6.a\n        FROM materialize.public.v2 JOIN db1.scm1.v6 ON v2.z = db1.scm1.v6.a;"

> CREATE VIEW "materialize"."public"."db_v_qual_test" AS SELECT "v2"."z", "db1"."scm1"."v6"."a" FROM "materialize"."public"."v2" JOIN "db1"."scm1"."v6" ON "v2"."z" = "db1"."scm1"."v6"."a"

# 🔬 Schema::Schema

# 🔬🔬 Require schema-level qualification
> CREATE DATABASE IF NOT EXISTS db1
> CREATE SCHEMA IF NOT EXISTS db1.scm1
> CREATE SCHEMA IF NOT EXISTS db1.scm2

> CREATE VIEW db1.scm1.v AS SELECT 1 as a;
> CREATE VIEW db1.scm2.v AS SELECT 2 as b;

# References to `v` are ambiguous because not all references are sufficiently
# qualified. This won't work until we have schema-level qualification.
> CREATE VIEW db1_scm1_scm2_min_qual_invalid AS
    SELECT * FROM
    (
        SELECT v.a FROM db1.scm1.v
    ) AS l
    JOIN (
        SELECT v.b FROM db1.scm2.v
    ) AS r
    ON l.a = r.b;

# ❌ Insufficient qualification
! ALTER VIEW db1.scm1.v rename to anything;
contains:renaming conflict
! ALTER VIEW db1.scm2.v rename to anything;
contains:renaming conflict

> DROP VIEW db1_scm1_scm2_min_qual_invalid

# References to `v` are ambiguous because not all references are sufficiently
# qualified.
> CREATE VIEW db1_scm1_scm2_mix_qual_invalid AS
    SELECT * FROM
    (
        SELECT db1.scm1.v.a FROM db1.scm1.v
    ) AS l
    JOIN (
        SELECT v.b FROM db1.scm2.v
    ) AS r
    ON l.a = r.b;

# ❌ Insufficient qualification
! ALTER VIEW db1.scm1.v rename to anything;
contains:renaming conflict
! ALTER VIEW db1.scm2.v rename to anything;
contains:renaming conflict

> DROP VIEW db1_scm1_scm2_mix_qual_invalid

# Qualifying to the point of differentiation allows renaming
> CREATE VIEW db1_scm1_scm2_valid_qual AS
    SELECT * FROM
    (
        SELECT scm1.v.a FROM db1.scm1.v
    ) AS l
    JOIN (
        SELECT scm2.v.b FROM db1.scm2.v
    ) AS r
    ON l.a = r.b;

# ✅ Sufficient qualification + new idents
> ALTER VIEW db1.scm1.v RENAME TO v3
> ALTER VIEW db1.scm1.v3 RENAME TO v4
> ALTER VIEW db1.scm2.v RENAME TO v5

> SHOW CREATE VIEW db1_scm1_scm2_valid_qual
name                                        create_sql
-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------
materialize.public.db1_scm1_scm2_valid_qual "CREATE VIEW\n    materialize.public.db1_scm1_scm2_valid_qual\n    AS\n        SELECT *\n        FROM\n            (SELECT scm1.v4.a FROM db1.scm1.v4) AS l\n                JOIN (SELECT scm2.v5.b FROM db1.scm2.v5) AS r ON l.a = r.b;"

> CREATE VIEW "materialize"."public"."db1_scm1_scm2_valid_qual_test" AS SELECT * FROM (SELECT "scm1"."v4"."a" FROM "db1"."scm1"."v4") AS "l" JOIN (SELECT "scm2"."v5"."b" FROM "db1"."scm2"."v5") AS "r" ON "l"."a" = "r"."b"

# 🔬🔬 No qualification necessary
> CREATE SCHEMA IF NOT EXISTS scm4;
> CREATE SCHEMA IF NOT EXISTS scm5;

> CREATE VIEW scm4.v1 AS
    SELECT 1 AS z;

> CREATE VIEW scm5.v2 AS
    SELECT 1 AS a;

> CREATE VIEW scm_scm_qual AS
    SELECT scm4.v1.z, scm5.v2.a
    FROM scm4.v1
    JOIN scm5.v2
    ON scm4.v1.z = scm5.v2.a;

# ❌ Identifiers used in dependent items
! ALTER VIEW scm4.v1 RENAME TO z;
contains:renaming conflict
! ALTER VIEW scm4.v1 RENAME TO scm5;
contains:renaming conflict
! ALTER VIEW scm4.v1 RENAME TO v2;
contains:renaming conflict
! ALTER VIEW scm4.v1 RENAME TO a;
contains:renaming conflict
! ALTER VIEW scm5.v2 RENAME TO a;
contains:renaming conflict
! ALTER VIEW scm5.v2 RENAME TO v1;
contains:renaming conflict

# ✅ New idents
> ALTER VIEW scm4.v1 RENAME TO v3;
> ALTER VIEW scm5.v2 RENAME TO v4;

> SHOW CREATE VIEW scm_scm_qual
name                            create_sql
-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------
materialize.public.scm_scm_qual "CREATE VIEW\n    materialize.public.scm_scm_qual\n    AS\n        SELECT scm4.v3.z, scm5.v4.a\n        FROM materialize.scm4.v3 JOIN materialize.scm5.v4 ON scm4.v3.z = scm5.v4.a;"

> CREATE VIEW "materialize"."public"."scm_scm_qual_test" AS SELECT "scm4"."v3"."z", "scm5"."v4"."a" FROM "materialize"."scm4"."v3" JOIN "materialize"."scm5"."v4" ON "scm4"."v3"."z" = "scm5"."v4"."a"

# 🔬 Schema::View
# Evaluate ability to handle one schema-qualified reference and one
# implicitly-qualified reference.

> CREATE VIEW v1 AS
    SELECT 1 AS z;

> CREATE VIEW scm_v_qual AS
    SELECT v1.z, scm5.v4.a
    FROM v1
    JOIN scm5.v4
    ON v1.z = scm5.v4.a;

# ❌ Identifiers used in dependent items
! ALTER VIEW v1 RENAME TO z;
contains:renaming conflict
! ALTER VIEW v1 RENAME TO scm5;
contains:renaming conflict
! ALTER VIEW v1 RENAME TO v4;
contains:renaming conflict

# ✅ New idents
> ALTER VIEW v1 RENAME TO v3;
> ALTER VIEW scm5.v4 RENAME TO v6;

> SHOW CREATE VIEW scm_v_qual
name                          create_sql
-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------
materialize.public.scm_v_qual "CREATE VIEW\n    materialize.public.scm_v_qual\n    AS\n        SELECT v3.z, scm5.v6.a\n        FROM materialize.public.v3 JOIN materialize.scm5.v6 ON v3.z = scm5.v6.a;"

> CREATE VIEW "materialize"."public"."scm_v_qual_test" AS SELECT "v3"."z", "scm5"."v6"."a" FROM "materialize"."public"."v3" JOIN "materialize"."scm5"."v6" ON "v3"."z" = "scm5"."v6"."a"

# 🔬 View::View
# Evaluate ability to handle two implicitly-qualified references.

> CREATE VIEW v4 AS
  SELECT 1 AS a

> CREATE VIEW v_v_qual AS
  SELECT v3.z, v4.a
  FROM v3
  JOIN v4
  ON v3.z = v4.a

# ❌ Identifiers used in dependent items
! ALTER VIEW v4 RENAME TO z;
contains:renaming conflict

# ✅ New idents
> ALTER VIEW v4 RENAME TO v6;
> ALTER VIEW v3 RENAME TO v5;

> SHOW CREATE VIEW v_v_qual
name                        create_sql
-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------
materialize.public.v_v_qual "CREATE VIEW\n    materialize.public.v_v_qual\n    AS SELECT v5.z, v6.a FROM materialize.public.v5 JOIN materialize.public.v6 ON v5.z = v6.a;"

> CREATE VIEW "materialize"."public"."v_v_qual_test" AS SELECT "v5"."z", "v6"."a" FROM "materialize"."public"."v5" JOIN "materialize"."public"."v6" ON "v5"."z" = "v6"."a"

# 🔬 Qualified wildcard

> CREATE VIEW qualified_wildcard AS
  SELECT v5.*
  FROM v5

# ✅ New idents
> ALTER VIEW v5 RENAME TO v7

> SHOW CREATE VIEW qualified_wildcard
name                                  create_sql
---------------------------------------------------------------------------------------------------------------------------------------------------------------
materialize.public.qualified_wildcard "CREATE VIEW materialize.public.qualified_wildcard AS SELECT v7.* FROM materialize.public.v7;"

> CREATE VIEW "materialize"."public"."qualified_wildcard_test" AS SELECT "v7".* FROM "materialize"."public"."v7"

# 🔬 WHERE...IN

> CREATE VIEW where_in_subquery AS
  SELECT b FROM t1 WHERE a IN (SELECT a FROM scm5.v6);

> CREATE VIEW where_in_literal AS
  SELECT b FROM t1 WHERE b IN ('v8')

# ❌ Identifiers used in dependent items
! ALTER VIEW where_in_subquery RENAME TO scm5
contains:renaming conflict

# ✅ New idents
> ALTER VIEW where_in_literal RENAME TO v8
> ALTER VIEW v8 RENAME TO v9

> SHOW CREATE VIEW v9
name                  create_sql
-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------
materialize.public.v9 "CREATE VIEW materialize.public.v9 AS SELECT b FROM materialize.public.t1 WHERE b IN ( 'v8' );"

> CREATE VIEW "materialize"."public"."where_in_literal_test" AS SELECT "b" FROM "materialize"."public"."t1" WHERE "b" IN ('v8')

# 🔬🔬🔬 Spaces

> CREATE VIEW space AS
  SELECT 1 as "has space";

> CREATE VIEW "already has space" AS
  SELECT 1 as my_space;

> CREATE VIEW space_dependent AS
  SELECT * FROM space
  JOIN "already has space"
  ON "already has space".my_space = space."has space";

# ❌ Identifiers used in own definition
! ALTER VIEW space RENAME TO "has space"
contains:renaming conflict
! ALTER VIEW "already has space" RENAME TO my_space
contains:renaming conflict

# ✅ New idents
> ALTER VIEW space RENAME TO "now has space"
> ALTER VIEW "already has space" RENAME TO "still has space"

> SHOW CREATE VIEW space_dependent
name                               create_sql
-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------
materialize.public.space_dependent "CREATE VIEW\n    materialize.public.space_dependent\n    AS\n        SELECT *\n        FROM\n            materialize.public.\"now has space\"\n                JOIN\n                    materialize.public.\"still has space\"\n                    ON \"still has space\".my_space = \"now has space\".\"has space\";"

> CREATE VIEW "materialize"."public"."space_dependent_test" AS SELECT * FROM "materialize"."public"."now has space" JOIN "materialize"."public"."still has space" ON "still has space"."my_space" = "now has space"."has space"

# 🔬 Keyword

> CREATE VIEW natural AS
  SELECT * FROM t1 NATURAL JOIN t1 AS a;

# ✅ Keywords are never trapped in a rename
> ALTER VIEW natural RENAME TO unnatural

> SHOW CREATE VIEW unnatural
name                         create_sql
-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------
materialize.public.unnatural "CREATE VIEW\n    materialize.public.unnatural\n    AS SELECT * FROM materialize.public.t1  NATURAL JOIN materialize.public.t1 AS a;"

> CREATE VIEW "materialize"."public"."unnatural_test" AS SELECT * FROM "materialize"."public"."t1" NATURAL JOIN "materialize"."public"."t1" AS "a"

# 🔬 Function names

> CREATE VIEW func AS
  SELECT count(1)

> CREATE VIEW no_func AS
  SELECT 1 AS a;

> CREATE VIEW func_dependency (a, x_a) AS
  SELECT * FROM no_func
  JOIN (
    SELECT * FROM no_func
  ) AS x
  ON no_func.a = x.a;

# ❌ Identifiers used in dependent items
! ALTER VIEW func RENAME TO count
contains:renaming conflict

# ✅ Non-colliding function name
> ALTER VIEW no_func RENAME TO count

> SHOW CREATE VIEW func_dependency
name                               create_sql
---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------
materialize.public.func_dependency "CREATE VIEW\n    materialize.public.func_dependency\n    (a, x_a)\n    AS\n        SELECT *\n        FROM\n            materialize.public.count\n                JOIN (SELECT * FROM materialize.public.count) AS x ON count.a = x.a;"

> CREATE VIEW "materialize"."public"."func_dependency_test" ("a", "x_a") AS SELECT * FROM count JOIN (SELECT * FROM count) AS x ON count.a = x.a

> SHOW CREATE VIEW count
name                     create_sql
---------------------------------------------------------------------------------------
materialize.public.count "CREATE VIEW materialize.public.count AS SELECT 1 AS a;"

> DROP VIEW count CASCADE;

> CREATE VIEW count AS SELECT 1

> CREATE TABLE j (b int)

> SHOW TABLES
name                   comment
-------------------------------
j              ""
renamed_mz_data_tbl    ""

> ALTER TABLE j RENAME TO renamed_j

> SHOW TABLES
name                   comment
-------------------------------
renamed_j              ""
renamed_mz_data_tbl    ""

> SELECT name FROM mz_tables WHERE id like 'u%';
name
------
renamed_j
renamed_mz_data_tbl

# Test that after renaming a materialized object it is possible to create
# another object with the original name. This used to fail because the index
# on the original object is not renamed. See database-issues#1585.
> CREATE TABLE t_orig ()
> ALTER TABLE t_orig RENAME TO t_dontcare
> CREATE TABLE t_orig ()
> CREATE VIEW v_orig AS SELECT 1
> CREATE DEFAULT INDEX ON v_orig
> ALTER VIEW v_orig RENAME TO v_dontcare
> CREATE VIEW v_orig AS SELECT 1
> CREATE DEFAULT INDEX ON v_orig

# Test renaming a schema that contains objects.

> CREATE SCHEMA to_be_renamed;

> SET SCHEMA TO to_be_renamed;

> CREATE SOURCE mz_data
  IN CLUSTER ${arg.single-replica-cluster}
  FROM KAFKA CONNECTION public.kafka_conn (TOPIC 'testdrive-data-${testdrive.seed}')

> CREATE TABLE mz_data_tbl FROM SOURCE mz_data (REFERENCE "testdrive-data-${testdrive.seed}")
  FORMAT AVRO USING SCHEMA '${writer-schema}'

> CREATE DEFAULT INDEX ON mz_data

> CREATE SINK sink1
  IN CLUSTER ${arg.single-replica-cluster}
  FROM mz_data_tbl
  INTO KAFKA CONNECTION public.kafka_conn (TOPIC 'testdrive-snk1-rename-schema-${testdrive.seed}')
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY CONNECTION public.csr_conn
  ENVELOPE DEBEZIUM

> SET SCHEMA TO public;

# Check the initial state of the create_sql.

> SHOW CREATE SOURCE to_be_renamed.mz_data;
name                                create_sql
---------------------------------------------------------------------------------------------------------------------------------------------------
materialize.to_be_renamed.mz_data "CREATE SOURCE materialize.to_be_renamed.mz_data\nIN CLUSTER <VARIABLE_OUTPUT>\nFROM KAFKA CONNECTION materialize.public.kafka_conn (TOPIC = '<VARIABLE_OUTPUT>')\nEXPOSE PROGRESS AS materialize.to_be_renamed.mz_data_progress;"

> SHOW CREATE SINK to_be_renamed.sink1;
name                                create_sql
---------------------------------------------------------------------------------------------------------------------------------------------------
materialize.to_be_renamed.sink1    "CREATE SINK materialize.to_be_renamed.sink1 IN CLUSTER <VARIABLE_OUTPUT> FROM materialize.to_be_renamed.mz_data_tbl INTO KAFKA CONNECTION materialize.public.kafka_conn (TOPIC = 'testdrive-snk1-rename-schema-${testdrive.seed}') FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY CONNECTION materialize.public.csr_conn ENVELOPE DEBEZIUM;"

# Make sure the create_sql got updated.

> ALTER SCHEMA to_be_renamed RENAME TO foo_bar;

> SHOW CREATE SOURCE foo_bar.mz_data;
name                                create_sql
---------------------------------------------------------------------------------------------------------------------------------------------------
materialize.foo_bar.mz_data "CREATE SOURCE materialize.foo_bar.mz_data\nIN CLUSTER <VARIABLE_OUTPUT>\nFROM KAFKA CONNECTION materialize.public.kafka_conn (TOPIC = '<VARIABLE_OUTPUT>')\nEXPOSE PROGRESS AS materialize.foo_bar.mz_data_progress;"

> SHOW CREATE SINK foo_bar.sink1;
name                                create_sql
---------------------------------------------------------------------------------------------------------------------------------------------------
materialize.foo_bar.sink1    "CREATE SINK materialize.foo_bar.sink1 IN CLUSTER <VARIABLE_OUTPUT> FROM materialize.foo_bar.mz_data_tbl INTO KAFKA CONNECTION materialize.public.kafka_conn (TOPIC = 'testdrive-snk1-rename-schema-${testdrive.seed}') FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY CONNECTION materialize.public.csr_conn ENVELOPE DEBEZIUM;"
