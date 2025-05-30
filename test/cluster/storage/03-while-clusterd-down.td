# Copyright Materialize, Inc. and contributors. All rights reserved.
#
# Use of this software is governed by the Business Source License
# included in the LICENSE file at the root of this repository.
#
# As of the Change Date specified in that file, in accordance with
# the Business Source License, use of this software will be governed
# by the Apache License, Version 2.0.

# Verify that the data ingested before `clusterd` crashed is still present but
# that newly ingested data does not appear.

# Increased from the default because of CI flakiness.
$ set-sql-timeout duration=180s

# With clusterd down, the upper of remote1 and remote2 will not advance. However, the global timestamp will advance.
# In strict serializable mode we may select a timestamp that is ahead of on of the sources uppers and hang forever.
> SET transaction_isolation = serializable

> SELECT * from remote1
one
two
three
> SELECT * from remote2
one
two
three

$ kafka-ingest format=bytes topic=remote1
four
$ kafka-ingest format=bytes topic=remote2
four

> SELECT * from remote1
one
two
three
> SELECT * from remote2
one
two
three

> SET transaction_isolation = 'strict serializable'
