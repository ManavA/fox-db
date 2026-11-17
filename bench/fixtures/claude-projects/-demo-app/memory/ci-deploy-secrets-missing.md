---
name: ci-deploy-secrets-missing
description: CI deploy job fails when signing secrets are not configured
---
The deploy workflow needs the signing secrets set in the CI environment; without them the upload step is skipped.
