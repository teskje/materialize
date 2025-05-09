# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

#
# Concurrent update on the same table.
#

$ postgres-execute connection=postgres://postgres:postgres@postgres
CREATE TABLE concurrent_update (f1 INTEGER, f2 TEXT, f3 INTEGER PRIMARY KEY);
ALTER TABLE concurrent_update REPLICA IDENTITY FULL;
INSERT INTO concurrent_update VALUES (1, 'r1', 0);
INSERT INTO concurrent_update VALUES (11, 'r2', 1);

$ schema-registry-wait topic=postgres.public.concurrent_update

> CREATE CONNECTION IF NOT EXISTS csr_conn TO CONFLUENT SCHEMA REGISTRY (
    URL '${testdrive.schema-registry-url}'
  );

> CREATE CONNECTION IF NOT EXISTS kafka_conn TO KAFKA (BROKER '${testdrive.kafka-addr}', SECURITY PROTOCOL PLAINTEXT);

> CREATE SOURCE concurrent_update
  FROM KAFKA CONNECTION kafka_conn (TOPIC 'postgres.public.concurrent_update');

> CREATE TABLE concurrent_update_tbl FROM SOURCE concurrent_update (REFERENCE "postgres.public.concurrent_update")
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY CONNECTION csr_conn
  ENVELOPE DEBEZIUM;

$ postgres-connect name=conn1 url=postgres://postgres:postgres@postgres

$ postgres-connect name=conn2 url=postgres://postgres:postgres@postgres

$ postgres-execute connection=conn1
BEGIN;
UPDATE concurrent_update SET f1 = f1 + 20 WHERE f1 = 1;

$ postgres-execute connection=conn2
BEGIN;
UPDATE concurrent_update SET f1 = f1 + 20 WHERE f1 = 11;

$ postgres-execute connection=conn1
UPDATE concurrent_update SET f1 = f1 * 2 WHERE f1 = 21;
COMMIT;

$ postgres-execute connection=conn2
UPDATE concurrent_update SET f1 = f1 * 10 WHERE f1 = 31;
COMMIT;

> SELECT f1, f2 FROM concurrent_update_tbl;
42 r1
310 r2
