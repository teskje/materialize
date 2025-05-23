# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

$ set-arg-default single-replica-cluster=quickstart

# Test that Protobuf message evolution is handled sensibly.

# Ingest one message with a very simple schema.

$ set schema
syntax = "proto3";

message Message {
    int32 i = 1;
}

$ file-append path=evolution.proto
\${schema}

$ protobuf-compile-descriptors inputs=evolution.proto output=evolution.pb set-var=evolution-schema

$ schema-registry-publish subject=testdrive-evolution-${testdrive.seed}-value schema-type=protobuf
\${schema}

$ kafka-create-topic topic=evolution partitions=1

> CREATE CONNECTION IF NOT EXISTS csr_conn TO CONFLUENT SCHEMA REGISTRY (
    URL '${testdrive.schema-registry-url}'
  );

> CREATE CONNECTION kafka_conn
  TO KAFKA (BROKER '${testdrive.kafka-addr}', SECURITY PROTOCOL PLAINTEXT);

> CREATE SOURCE evolution
  IN CLUSTER ${arg.single-replica-cluster}
  FROM KAFKA CONNECTION kafka_conn (TOPIC 'testdrive-evolution-${testdrive.seed}')

> CREATE TABLE evolution_tbl FROM SOURCE evolution (REFERENCE "testdrive-evolution-${testdrive.seed}")
  FORMAT PROTOBUF USING CONFLUENT SCHEMA REGISTRY CONNECTION csr_conn

$ kafka-ingest topic=evolution format=protobuf descriptor-file=evolution.pb message=Message confluent-wire-format=true
{"i": 1}

> SELECT * FROM evolution_tbl
i
---
1

# Adding new fields and renaming existing fields is safe. New fields won't be
# reflected in the source, though.

$ set schema
syntax = "proto3";

message Message {
    int32 i_renamed = 1;
    bool extra = 2;
}

$ file-delete path=evolution.proto
$ file-append path=evolution.proto
\${schema}

$ protobuf-compile-descriptors inputs=evolution.proto output=evolution.pb set-var=evolution-schema

$ schema-registry-publish subject=testdrive-evolution-${testdrive.seed}-value schema-type=protobuf
\${schema}

$ kafka-ingest topic=evolution format=protobuf descriptor-file=evolution.pb message=Message confluent-wire-format=true
{"i_renamed": 2, "extra": false}

> SELECT * FROM evolution_tbl
i
---
1
2

# Changing the size of an integer type is allowed; values are truncated/promoted
# as in C.

$ set schema
syntax = "proto3";

message Message {
    bool i_demoted = 1;
}

$ file-delete path=evolution.proto
$ file-append path=evolution.proto
\${schema}

$ protobuf-compile-descriptors inputs=evolution.proto output=evolution.pb set-var=evolution-schema

$ schema-registry-publish subject=testdrive-evolution-${testdrive.seed}-value schema-type=protobuf
\${schema}

$ kafka-ingest topic=evolution format=protobuf descriptor-file=evolution.pb message=Message confluent-wire-format=true
{"i_demoted": false}

> SELECT * FROM evolution_tbl
i
---
1
2
0

# Expect that ingesting an incompatible message will brick the topic. We have to
# explicitly disable the schema registry's protection against this.

$ http-request method=PUT content-type=application/json
  url=${testdrive.schema-registry-url}config/testdrive-evolution-${testdrive.seed}-value
{"compatibility": "NONE"}

$ set schema
syntax = "proto3";

message Message {
    string wrong = 1;
}

$ file-delete path=evolution.proto
$ file-append path=evolution.proto
\${schema}

$ protobuf-compile-descriptors inputs=evolution.proto output=evolution.pb set-var=evolution-schema

$ schema-registry-publish subject=testdrive-evolution-${testdrive.seed}-value schema-type=protobuf
\${schema}

$ kafka-ingest topic=evolution format=protobuf descriptor-file=evolution.pb message=Message confluent-wire-format=true
{"wrong": "i'm not an int!"}

! SELECT * FROM evolution_tbl
contains:Decode error: protobuf deserialization error: failed to decode Protobuf message: invalid wire type: LengthDelimited (expected Varint)
