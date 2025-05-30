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
# Attempt to introduce NULL values into a source that does not accept nulls - should fail
#

$ set not-null={"type": "record", "name": "schema_union", "fields": [ {"name": "f1", "type": ["int" ] } ] }
$ set null={"type": "record", "name": "schema_union", "fields": [ {"name": "f1", "type": ["null", "int" ] } ] }

$ kafka-create-topic topic=resolution-unions

$ kafka-ingest format=avro topic=resolution-unions schema=${not-null} timestamp=1
{"f1": {"int": 123 } }

> CREATE CONNECTION IF NOT EXISTS csr_conn TO CONFLUENT SCHEMA REGISTRY (
    URL '${testdrive.schema-registry-url}'
  );

> CREATE CONNECTION kafka_conn
  TO KAFKA (BROKER '${testdrive.kafka-addr}', SECURITY PROTOCOL PLAINTEXT);

> CREATE SOURCE resolution_unions
  IN CLUSTER ${arg.single-replica-cluster}
  FROM KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-resolution-unions-${testdrive.seed}')

> CREATE TABLE resolution_unions_tbl FROM SOURCE resolution_unions (REFERENCE "testdrive-resolution-unions-${testdrive.seed}")
  FORMAT AVRO USING CONFLUENT SCHEMA REGISTRY CONNECTION csr_conn
  ENVELOPE NONE

$ kafka-ingest format=avro topic=resolution-unions schema=${null} timestamp=2
{"f1": null }
{"f1": {"int": 123 } }

! SELECT f1 FROM resolution_unions_tbl
contains:Failed to match writer union variant `Null` against any variant in the reader for field `schema_union.f1`

! SELECT f1 FROM resolution_unions_tbl
contains:unable to decode row (Avro schema id =
