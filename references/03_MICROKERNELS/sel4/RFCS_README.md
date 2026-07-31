<!--
  SPDX-License-Identifier: CC-BY-SA-4.0
  Copyright 2020 seL4 Project a Series of LF Projects, LLC.
-->

# seL4 Request for Comments (RFC) Process

> - [Approved RFCs](https://sel4.github.io/rfcs/)
> - [Open RFCs](https://github.com/seL4/rfcs/pulls?q=is%3Aopen+is%3Apr+-label%3Anot-rfc+)
> - [RFCs under implementation](https://github.com/seL4/rfcs/tree/main/src/active)

To be notified of new RFCs, use the GitHub button on the top to watch pull
requests on this repository.

The seL4 foundation uses the request for comments (RFC) process to

- allow the community to discuss design changes in seL4,
- gather valuable feedback from the community on changes the foundation is
  considering,
- allow members of the seL4 community to get support and approval to propose and
  implement their own changes to the seL4 ecosystem,
- ensure that all changes made to core components of the seL4 ecosystem or that
  have wide and varying impacts on users of seL4 undergo rigorous review, and
- ensure large changes are well advertised and can viewed publicly before
  contributors commit to implementing them.

This helps the seL4 community ensure that such changes are made with the goal of
the best outcome for the most users of seL4 without compromising seL4's
high-assurance properties of functional correctness, isolation, and security.

- [When to follow the RFC process](#when-to-follow-the-rfc-process)
- [When not to follow the RFC process](#when-not-to-follow-the-rfc-process)
- [The RFC Process](#the-rfc-process)
  - [Before creating a new RFC](#before-creating-a-new-rfc)
  - [Proposing an new RFC](#proposing-an-new-rfc)
  - [Getting an RFC approved](#getting-an-rfc-approved)
  - [Postponement of an RFC](#postponement-of-an-rfc)
  - [Stage 1 approval](#stage-1-approval)
  - [Implementation of an approved RFC](#implementation-of-an-approved-rfc)
  - [Deferment of an approved RFC](#deferment-of-an-approved-rfc)
- [RFC states](#rfc-states)
  - [Under discussion](#under-discussion)
  - [Ready for implementation](#ready-for-implementation)
  - [Closed](#closed)

## When to follow the RFC process

All substantial changes to the seL4 ecosystem must be made using the RFC
process. Substantial changes are those that that impact a large number of users
of seL4 in a way that may require them to change their own projects, changes the
operation of toolchain used to build projects on seL4, changes the underlying
model of one or more software components within the ecosystem.

Examples of changes that must follow the RFC process include:

- Removing support for a platform
- Adding support for a new architecutre to the kernel
- Changing the versioning system used for libraries
- Adding a new API feature to the kernel
- Adding a new operation/invocation to an existing capability type, or removing one
- Adding or removing a capability or object type
- Adding or removing a library or framework to/from the set of repositories the
  foundation maintains

If you try to make a substantial change via a pull request alone, your request
is likely to be rejected and you will be asked to use the RFC process to propose
your change instead. It is Ok to make draft pull request in conjunction with an
RFC or link to an implementation in another repository if you want to provide a
prototype implementation for helping the RFC discussion. In that case, please
indicate so in the pull request and provides links in RFC and PR.

## When not to follow the RFC process

Changes such as bug fixes, refactoring, optimisations, or those that do not
affect the functional requirements of the kernel, supporting tooling,
infrastructure, or system components can be made through the existing pull
request process on the relevant repository.

Examples of changes that should not involve the RFC process include:

- Fixing a typo in existing code,
- Clarifying code or documentation,
- Adding a driver to an existing layer of the system
- Refactoring code without affecting functionality or verification

If you are unsure whether a change requires an RFC, ask on the [seL4 Mattermost]
chat or post a question to the [development mailing list] or [discourse forum].


## The RFC Process

### Before creating a new RFC

Before proposing a new RFC it is important to determine whether it will be
supported and what possible options exist to solve the particular problem.

This should be done through discussion on one of seL4 [community forums].
Posting a partial pre-RFC write-up on the [RFC discourse forum] can be
a good way to get the discussion started and get people interested in your idea.

This part of the process should help you determine whether your idea has already
been proposed, whether or not it fits with the near-term goals of the ecosystem,
and how to propose it with the best chances of acceptance.

You should make sure that during this discussion you address as many of the
sections in the RFC [template] as you can. This will ensure that your proposal is
well prepared before it is formally presented as an RFC and will speed up much
of the RFC process.

Once you have the support of some existing seL4 developers they can help you
take your idea through the rest of the RFC process.

After discussion, RFCs will ultimately be approved, postponed, or rejected by
a decision of the [Technical Steering Committee][TSC] of the seL4 Foundation.

[seL4 Mattermost]: https://mattermost.trustworthy.systems/sel4-external/ "seL4 Mattermost"
[development mailing list]: https://lists.sel4.systems/postorius/lists/devel.sel4.systems/ "seL4 development mailing list"
[discourse forum]: https://sel4.discourse.group "seL4 discourse forum"
[RFC discourse forum]: https://sel4.discourse.group/c/rfc-discussion/
[community forums]: https://sel4.systems/contact/
[TSC]: https://sel4.systems/Foundation/TSC/ "seL4 Technical Steering Committee"
[template]: 0000-template.md "RFC template"


### Proposing an new RFC

An RFC exists in the form of a pull request on the seL4 [RFC repository]. Anyone
can propose an RFC.

You should check the [postponed RFCs] to see if your idea has already been
proposed but was previously postponed. You can adopt a postponed RFC which will
return it to the proposal stage and you will become responsible for it.

Once you create an RFC, you are the one responsible for maintaining it
throughout the approval process and making sure it makes progress. You will be
able to modify it in response to discussion and feedback up until the point it
is either approved or rejected.

Every RFC should use the RFC [template], omitting sections that are not relevant
for the RFC (most sections will be relevant, do not omit them lightly).

[RFC repository]: https://github.com/seL4/rfcs "seL4 RFC repository"
[postponed RFCs]: https://github.com/seL4/rfcs/pulls?q=is%3Aclosed+is%3Apr+label%3Apostponed


### Getting an RFC approved

After you propose an RFC, it is likely to undergo several rounds of changes in
response to the discussion on the pull request. Anyone is allowed to engage in
this discussion.

Once the discussion phase is concluded, for instance because consensus has been
reached or it has become clear that there will not be a consensus, the
[Technical Steering Committee][TSC] of the seL4 Foundation will make a decision
on the RFC, usually in an online video meeting, sometimes via email.

The Steering Committee will either give stage 1 approval, fully approve,
postpone, defer, require changes, or reject the RFC. When this occurs, a
_disposition_ will be added to the RFC outlining the reason for the particular
ruling. If the discussion of an RFC has been particularly long, a summary
comment will be made on the RFC before the given ruling is made.


### Postponement of an RFC

When an RFC has a moderate amount of support and is generally accepted as a good
idea but lacks someone to shepherd it to the point where it can be accepted it
is _postponed_.

This indicates that the RFC may be revived in the future when someone can be
found to take it through the remainder of the approval process. Anyone may
_adopt_ a postponed RFC if they wish to see it approved and want to rally
support for it.

[Postponed RFCs] are pull requests that are closed with the postponed label.
Reopening the pull request means you wish to adopt it, restart discussion, and
develop it further.

### Stage 1 approval

In some cases, an RFC may require an implementation or very detailed design to
properly judge its merits. When the TSC decides that an RFC is likely to get
accepted if a convincing implementation can be shown, the TSC may give _stage 1
approval_ for this RFC. Stage 1 approval means a candidate implementation should
go ahead, but will need to be reviewed again, e.g. for performance and
usability, before it can be fully accepted.

RFCs with stage 1 approval are open pull requests with the stage 1 approval
label.

### Implementation of an approved RFC

Once the Steering Committee approves an RFC, the corresponding pull request is
merged and the RFC marked as _active_. The TSC may assign it to a contributor
for implementation (e.g. the RFC proposer), may create an issue for tracking
implementation, or continue more detailed implementation discussion on a
corresponding draft pull request if that already exists.

The implementation of an RFC may lead to the resolution of many issues that were
present in the RFC when it was approved. As such, the RFC may be subject to
further changes throughout this part of the process. These changes should not
change the overall goals or design of the RFC but should merely resolve issues
or extend the design to cope with unanticipated corner cases. They should be
reflected in the RFC text via additional pull requests to the [RFC repository].

When implementation is complete, the RFC is marked as _implemented_.

### Deferment of an approved RFC

There may not be sufficient resources to implement an RFC when it is approved or
resources may be diverted from the implementation of an RFC when higher priority
work arises. In either case, the RFC will be _deferred_. A _deferred_ RFC is
lower priority than an _active_ one and will generally see no progress on its
implementation.

Anyone can request that a _deferred_ RFC be given priority be demonstrating
increased demand or urgency for its implementation, volunteering to work on the
implementation, or find funding for the Foundation to implement it.

## RFC states

In summary, an sel4 RFC can be in the following states:

### Under discussion

- **open:** a new RFC, open pull request with ongoing discussion
- **stage 1 approved:** likely enough to be accepted for detailed design, open
  pull request with label _stage 1 approval_.

### Ready for implementation

- **approved:** approved for implementation, merged RFC pull request, marked as active,
  should have tracking issue or draft implementation PR with an assignee
- **deferred:** approved for implementation, but not currently being worked on,
  merged RFC pull request, marked as deferred

### Closed

- **implemented:** work on this RFC is finished, closed RFC PR and implementation,
  marked as implemented
- **postponed:** closed RFC pull request, can be reopened, labelled as postponed
- **rejected:** closed RFC pull request, labelled as rejected
