// Worked example for docs/security.md (ADR 0013): the policy file.

// The web team's CI may index and prune its own repo, and nothing else.
@id("web-ci")
permit(principal == Token::"web-ci", action in [Action::"index", Action::"prune"], resource == Repo::"acme/web");

// acme readers read and search every acme repo except the legal one.
@id("acme-read")
permit(principal in Group::"acme-readers", action in [Action::"read", Action::"search"], resource in Org::"acme");
@id("legal-hidden")
forbid(principal in Group::"acme-readers", action in [Action::"read", Action::"search"], resource == Repo::"acme/legal");

// Operators run the cluster and may read everything.
@id("ops")
permit(principal in Group::"operators", action in [Action::"status", Action::"snapshot", Action::"admin", Action::"read", Action::"search"], resource);

// Nobody writes to the frozen legal repo.
@id("legal-frozen")
forbid(principal, action in [Action::"index", Action::"prune", Action::"delete"], resource == Repo::"acme/legal");
