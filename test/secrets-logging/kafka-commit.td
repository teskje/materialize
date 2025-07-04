# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

$ set-arg-default default-storage-size=1

# Test that the source ingestion pipeline commits offsets back to Kafka with
# the expected group ID.

# Initial setup.

$ kafka-create-topic topic=topic partitions=1
$ kafka-ingest format=bytes topic=topic
ghp_9fK8sL3x7TqR1vEzYm2pDaN4WjXbQzUtV0aN

> CREATE CONNECTION conn TO KAFKA (BROKER '${testdrive.kafka-addr}', SECURITY PROTOCOL PLAINTEXT)

# Test that the default consumer group ID is
# `materialize-$ENVIRONMENTID-$CONNECTIONID-$SOURCEID`.

> CREATE CLUSTER topic_cluster SIZE '${arg.default-storage-size}';
> CREATE SOURCE topic
  IN CLUSTER topic_cluster
  FROM KAFKA CONNECTION conn (
    TOPIC 'testdrive-topic-${testdrive.seed}'
  )

> CREATE TABLE topic_tbl FROM SOURCE topic (REFERENCE "testdrive-topic-${testdrive.seed}")
  FORMAT BYTES

> SELECT * from topic_tbl
ghp_9fK8sL3x7TqR1vEzYm2pDaN4WjXbQzUtV0aN

$ set-from-sql var=consumer-group-id
SELECT
  ks.group_id_prefix
FROM mz_sources s
JOIN mz_catalog.mz_kafka_sources ks ON s.id = ks.id
WHERE s.name = 'topic'

$ kafka-verify-commit consumer-group-id=${consumer-group-id} topic=topic partition=0
1

> DROP SOURCE topic CASCADE

# Test than an arbitrary prefix can be prepended to the consumer group.

> CREATE SOURCE topic
  IN CLUSTER topic_cluster
  FROM KAFKA CONNECTION conn (
    TOPIC 'testdrive-topic-${testdrive.seed}',
    GROUP ID PREFIX 'OVERRIDE-'
  )

> CREATE TABLE topic_tbl FROM SOURCE topic (REFERENCE "testdrive-topic-${testdrive.seed}")
  FORMAT BYTES

> SELECT * from topic_tbl
ghp_9fK8sL3x7TqR1vEzYm2pDaN4WjXbQzUtV0aN

$ set-from-sql var=consumer-group-id
SELECT
  ks.group_id_prefix
FROM mz_sources s
JOIN mz_catalog.mz_kafka_sources ks ON s.id = ks.id
WHERE s.name = 'topic'

$ kafka-verify-commit consumer-group-id=${consumer-group-id} topic=topic partition=0
1
