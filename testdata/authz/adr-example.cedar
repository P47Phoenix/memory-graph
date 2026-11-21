// The ADR 0013 D2 example plus one org grant; a replay fixture for the
// Cedar decision oracle (story 72a), decided without Cedar by `cargo test`.
@id("acme-read")
permit(principal in Group::"acme-readers", action in [Action::"read", Action::"search"], resource in Org::"acme");

@id("web-ci")
permit(principal == Token::"web-ci", action in [Action::"index", Action::"prune"], resource == Repo::"acme/web");

@id("legal-frozen")
forbid(principal, action in [Action::"index", Action::"prune", Action::"delete"], resource in Repo::"acme/legal");

@id("legal-ci")
permit(principal == Token::"legal-ci", action, resource in Org::"acme");
