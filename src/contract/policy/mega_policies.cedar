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

// root admin can do anything
permit (principal, action, resource)
when { principal == User::"genedna" || principal == User::"benjamin-747" };