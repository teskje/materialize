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
# Attempt to write a concrete type with a union source without a match
#

$ set union-int={"type": "record", "name": "schema_union", "fields": [ {"name": "f1", "type": [ "int" ] } ] }
$ set concrete-double={"type": "record", "name": "schema_union", "fields": [ {"name": "f1", "type": "double" } ] }

$ kafka-create-topic topic=resolution-union-concrete

$ kafka-ingest format=avro topic=resolution-union-concrete schema=${union-int} timestamp=1
{"f1": {"int": 123 } }

> CREATE CONNECTION IF NOT EXISTS csr_conn TO CONFLUENT SCHEMA REGISTRY (
    URL '${testdrive.schema-registry-url}'
  );

> CREATE CONNECTION kafka_conn
  TO KAFKA (BROKER '${testdrive.kafka-addr}', SECURITY PROTOCOL PLAINTEXT);

> CREATE SOURCE resolution_union_concrete
  IN CLUSTER ${arg.single-replica-cluster}
  FROM KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-resolution-union-concrete-${testdrive.seed}')

> CREATE TABLE resolution_union_concrete_tbl FROM SOURCE resolution_union_concrete (REFERENCE "testdrive-resolution-union-concrete-${testdrive.seed}")
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY CONNECTION csr_conn
  ENVELOPE NONE

$ kafka-ingest format=avro topic=resolution-union-concrete schema=${concrete-double} timestamp=2
{"f1": 123.456 }

! SELECT f1 FROM resolution_union_concrete_tbl
contains:No matching schema in reader union for writer type `Double` for field `schema_union.f1`

! SELECT f1 FROM resolution_union_concrete_tbl
contains:failed to resolve Avro schema (id =
