# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

$ set-regex match=cluster1|quickstart replacement=<CLUSTER_NAME>

> CREATE VIEW v AS VALUES (1, 'foo'), (2, 'bar'), (3, 'foo'), (1, 'bar')

> SELECT * FROM v
1 "foo"
1 "bar"
2 "bar"
3 "foo"

> CREATE TEMPORARY VIEW temp_v AS SELECT * FROM v

> SELECT * FROM temp_v
1 "foo"
1 "bar"
2 "bar"
3 "foo"

> SELECT * FROM mz_temp.temp_v
1 "foo"
1 "bar"
2 "bar"
3 "foo"

! CREATE VIEW non_temp AS SELECT * FROM temp_v
contains:non-temporary items cannot depend on temporary item

> CREATE TEMP VIEW double_temp_v AS SELECT * FROM temp_v

! CREATE TEMP VIEW double_temp_v AS SELECT * FROM temp_v
contains:view "mz_temp.double_temp_v" already exists

> CREATE OR REPLACE TEMP VIEW double_temp_v AS SELECT * FROM temp_v

! CREATE OR REPLACE VIEW double_temp_v AS SELECT * FROM temp_v
contains:non-temporary items cannot depend on temporary item

> SELECT * FROM double_temp_v
1 "foo"
1 "bar"
2 "bar"
3 "foo"

> SELECT * FROM mz_temp.double_temp_v
1 "foo"
1 "bar"
2 "bar"
3 "foo"

# A temporary view should mask a normal view with the same name

> CREATE VIEW v1 AS SELECT 1 AS f1;

> CREATE TEMPORARY VIEW v1 AS SELECT 2 AS f2;

> SELECT * FROM v1;
 f2
----
  2

> DROP VIEW v1;

> SELECT * FROM v1;
 f1
----
  1

> DROP VIEW v1;

! SELECT * FROM v1;
contains:unknown catalog item 'v1'

# Rename temporary view

> CREATE TEMPORARY VIEW v1 AS SELECT 1 AS f1;

> ALTER VIEW v1 RENAME TO v2;

! SELECT * FROM v1;
contains:unknown catalog item 'v1'

> SELECT * FROM v2;
 f1
----
  1

> DROP VIEW v2;

#####################################################################
# Temporary tables

> CREATE TEMPORARY TABLE temp_t (a int, b text NOT NULL)

! CREATE TEMP TABLE temp_t (a int, b text NOT NULL)
contains:table "mz_temp.temp_t" already exists

> INSERT INTO temp_t VALUES (1, 'testing')

> SHOW INDEXES ON temp_t

# Blocked on https://github.com/MaterializeInc/database-issues/issues/1017.
#
# > CREATE TEMPORARY DEFAULT INDEX ON temp_t
#
# > SHOW INDEXES ON temp_t
#  on_name    key_name            seq_in_index  column_name  expression  nullable
#  name                 on          cluster         key
# --------------------------------------------------------------------------------
#  temp_t_primary_idx   d           quickstart      {a, b}

> DROP TABLE temp_t

# A temporary table should mask a normal table with the same name

> CREATE TABLE t1 (f1 INTEGER);

> INSERT INTO t1 VALUES (1);

> CREATE TEMPORARY TABLE t1 (f2 INTEGER);

> INSERT INTO t1 VALUES (2);

> SELECT * FROM t1;
 f2
----
  2

> SELECT * FROM public.t1;
 f1
----
  1

> DROP TABLE t1;

> SELECT * FROM t1;
 f1
----
  1

> DROP TABLE t1;

# Rename temporary table

> CREATE TEMPORARY TABLE t1 (f1 INTEGER);

> INSERT INTO t1 VALUES (1);

> ALTER TABLE t1 RENAME TO t2;

! SELECT * FROM t1;
contains:unknown catalog item 't1'

> SELECT * FROM t2;
 f1
----
  1

> DROP TABLE t2;

# A non-temporary view can not depend on a temporary table

> CREATE TEMPORARY TABLE t1 (f1 INTEGER);

! CREATE VIEW non_temp AS SELECT * FROM t1;
contains:non-temporary items cannot depend on temporary item

! CREATE VIEW non_temp AS SELECT (SELECT * FROM t1);
contains:non-temporary items cannot depend on temporary item

> DROP TABLE t1;

#####################################################################
# Make sure the mz_temp schema is protected

! DROP SCHEMA mz_temp
contains:cannot drop schema mz_temp because it is required by the database system

! CREATE TABLE mz_temp.table_in_mz_temp (f1 INTEGER)
contains:unknown schema 'mz_temp'

#####################################################################
# Test things we shouldn't be able to make temporary.

##### Temporary sources.
$ set schema={
    "type": "record",
    "name": "envelope",
    "fields": [{},]
  }

> CREATE CONNECTION kafka_conn
  TO KAFKA (BROKER '${testdrive.kafka-addr}', SECURITY PROTOCOL PLAINTEXT);

! CREATE TEMPORARY SOURCE data_schema_inline
  FROM KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-data-${testdrive.seed}')
  FORMAT AVRO USING SCHEMA '${schema}'
  ENVELOPE DEBEZIUM
contains:Expected TABLE, or VIEW after CREATE TEMPORARY, found SOURCE

> CREATE CONNECTION IF NOT EXISTS csr_conn TO CONFLUENT SCHEMA REGISTRY (
    URL '${testdrive.schema-registry-url}'
  );

##### Temporary sinks.
! CREATE TEMPORARY SINK data_sink FROM data
  INTO KAFKA BROKER '${testdrive.kafka-addr}' TOPIC 'testdrive-data-sink-${testdrive.seed}'
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY CONNECTION csr_conn
contains:Expected TABLE, or VIEW after CREATE TEMPORARY, found SINK

#####################################################################

! DROP VIEW temp_v;
contains:cannot drop view "temp_v": still depended upon by view "double_temp_v"

> DROP VIEW double_temp_v;

! SELECT * FROM double_temp_v;
contains:unknown catalog item 'double_temp_v'

> DISCARD TEMP

> SELECT * FROM mz_indexes WHERE name = 'foo_primary_idx'

> SELECT * FROM mz_indexes WHERE name = 'temp_t_primary_idx'

! SELECT * FROM temp_v;
contains:unknown catalog item 'temp_v'

! CREATE TEMP VIEW mz_catalog.a AS SELECT 1
contains:cannot create temporary item in non-temporary schema
