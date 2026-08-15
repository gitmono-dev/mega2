// policy "anyoneCanViewPublicRepo"
//
// `pushRepo` is deliberately absent (UN-09): a repository being public says
// anyone may *read* it, not that anyone may write to it. Pushing is granted by
// role below.
permit (
    principal,
    action in
        [Action::"viewRepo",
         Action::"pullRepo",
         Action::"forkRepo",
         Action::"openIssue",
         Action::"createMergeRequest"],
    resource
)
unless { resource.is_private };

//Actions for readers
//
// Read access plus the ability to propose work; `pushRepo` moved to the
// maintainer set (UN-09), since a reader writing to the repository was never
// the intent of the role name.
permit (
    principal,
    action in
        [Action::"viewRepo",
         Action::"pullRepo",
         Action::"forkRepo",
         Action::"openIssue",
         Action::"createMergeRequest"],
    resource
)
when { principal in resource.readers };

//Actions for maintainers
permit (
    principal,
    action in
        [Action::"pushRepo",
         Action::"editIssue",
         Action::"editMergeRequest",
         Action::"assignIssue",
         Action::"approveMergeRequest"],
    resource
)
when { principal in resource.maintainers };

//Actions for admins
//
// `pushRepo` is listed here as well as in the maintainer set: admin inherits
// the maintainer group in the seeded hierarchy, but the grant must not depend
// on an ACL keeping that inheritance in place.
permit (
    principal,
    action in
        [Action::"pushRepo",
         Action::"addMaintainer",
         Action::"addAdmin",
         Action::"deleteRepo",
         Action::"deleteIssue",
         Action::"deleteMergeRequest"],
    resource
)
when { principal in resource.admins };

// UN-04: there is no hardcoded root admin.
//
// Two named individuals used to be permitted everything on every resource,
// which made the policy file a second, invisible source of admin power: an
// operator reading the ACL could not tell who was actually privileged, and
// removing someone from the admin group did not remove their access. Admin is
// now exactly membership of `UserGroup::"admin"` in the entity store — seeded
// from `monorepo.admin` into `/.mega_cedar.json` — so the ACL is the whole
// answer (ADR-UN-03).