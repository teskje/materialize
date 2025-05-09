# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

$ set-arg-default single-replica-cluster=quickstart

#
# Make sure that FETCH-ing using multiple cursors from different sources works as expected
#

$ postgres-execute connection=postgres://mz_system:materialize@${testdrive.materialize-internal-sql-addr}
ALTER SYSTEM SET min_timestamp_interval = '100ms'

$ set int={"type": "record", "name": "field_int", "fields": [ {"name": "f1", "type": "int"} ] }

$ kafka-create-topic topic=fetch-concurrent-two-sources

> CREATE CONNECTION kafka_conn
  TO KAFKA (BROKER '${testdrive.kafka-addr}', SECURITY PROTOCOL PLAINTEXT);

> CREATE SOURCE fetch_concurrent_two_sources1
  IN CLUSTER ${arg.single-replica-cluster}
  FROM KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-fetch-concurrent-two-sources-${testdrive.seed}')
  FORMAT AVRO USING SCHEMA '${int}'
  ENVELOPE NONE
  WITH (TIMESTAMP INTERVAL '100ms')

> CREATE SOURCE fetch_concurrent_two_sources2
  IN CLUSTER ${arg.single-replica-cluster}
  FROM KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-fetch-concurrent-two-sources-${testdrive.seed}')
  FORMAT AVRO USING SCHEMA '${int}'
  ENVELOPE NONE
  WITH (TIMESTAMP INTERVAL '100ms')

> CREATE MATERIALIZED VIEW fetch_concurrent_two_sources1_view AS SELECT * FROM fetch_concurrent_two_sources1 ORDER BY f1;

> CREATE MATERIALIZED VIEW fetch_concurrent_two_sources2_view AS SELECT * FROM fetch_concurrent_two_sources2 ORDER BY f1;


$ kafka-ingest format=avro topic=fetch-concurrent-two-sources schema=${int} timestamp=1
{"f1": 12}

$ kafka-ingest format=avro topic=fetch-concurrent-two-sources schema=${int} timestamp=2
{"f1": 23}

$ kafka-ingest format=avro topic=fetch-concurrent-two-sources schema=${int} timestamp=3
{"f1": 34}

> SELECT COUNT(*) FROM fetch_concurrent_two_sources1_view;
3

> SELECT COUNT(*) FROM fetch_concurrent_two_sources2_view;
3

> BEGIN

> DECLARE c1 CURSOR FOR SELECT * FROM fetch_concurrent_two_sources1_view;

> DECLARE c2 CURSOR FOR SELECT * FROM fetch_concurrent_two_sources2_view;

> FETCH 1 c1;
12

> FETCH 1 c2;
12

> FETCH 1 c1;
23

> FETCH 1 c2;
23

> FETCH 1 c1;
34

> FETCH 1 c2;
34
