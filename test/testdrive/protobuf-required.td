# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

$ set-arg-default single-replica-cluster=quickstart

# Test that required Protobuf fields are unsupported.

$ file-append path=required.proto
syntax = "proto2";

message Required {
  required int32 f = 1;
}

$ protobuf-compile-descriptors inputs=required.proto output=required.pb set-var=required-schema

$ kafka-create-topic topic=required partitions=1

$ kafka-ingest topic=required format=protobuf descriptor-file=required.pb message=Required
{"f": 42}
> CREATE CONNECTION kafka_conn
  TO KAFKA (BROKER '${testdrive.kafka-addr}', SECURITY PROTOCOL PLAINTEXT);

> CREATE SOURCE required
  IN CLUSTER ${arg.single-replica-cluster}
  FROM KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-required-${testdrive.seed}')

> CREATE TABLE required_tbl FROM SOURCE required (REFERENCE "testdrive-required-${testdrive.seed}")
  FORMAT PROTOBUF MESSAGE '.Required' USING SCHEMA '${required-schema}'

> SELECT * FROM required_tbl
f
----
42

$ kafka-ingest topic=required format=protobuf descriptor-file=required.pb message=Required
{}

! SELECT * FROM required_tbl
contains:protobuf message missing required field f
