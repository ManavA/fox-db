---
name: staging-database
description: never hand-edit the shared staging database; run the migration job and read its logs
---
Schema changes reach staging only through the migration job; check its log output afterwards.
