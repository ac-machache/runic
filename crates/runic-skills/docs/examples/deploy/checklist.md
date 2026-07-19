# Pre-flight checklist

- [ ] Release notes exist for this version and match the diff.
- [ ] Database migrations, if any, are backward-compatible with the
      currently running version (the old code must survive the new schema).
- [ ] Feature flags for new behavior default to OFF.
- [ ] The previous version's artifact is still available for rollback.
- [ ] On-call has been notified of the deploy window.
- [ ] No ongoing incident on this service or its hard dependencies.
