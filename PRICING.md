# kcore Pricing

kcore keeps the core platform open source under Apache-2.0. Community Edition ISOs and `kctl` binaries are published on [GitHub Releases](https://github.com/kcorehypervisor/kcore/releases). Paid subscriptions provide the official supported distribution, stable production updates, official signed ISO images, and commercial support.

For how licensing relates to subscriptions, see [Licensing & editions](https://kcorehypervisor.com/docs/user/licensing.html) on the product site.

## Community Edition

- Apache-2.0
- Download ISO & kctl from GitHub Releases (or build from source)
- Suitable for contributors, labs, homelabs, and evaluation
- No subscription required

## Standard

- £249 / year / CPU socket
- Official signed ISO images
- Stable production update channel
- Tested updates
- Support via customer portal or support email
- 5 support tickets per year
- 1 business day response time

## Premium

- £599 / year / CPU socket
- Official signed ISO images
- Stable production update channel
- Tested updates
- Support via customer portal or support email
- Unlimited support tickets
- 4-hour response time within a business day
- Remote support included

## Enterprise

- £999 / year / CPU socket (minimum 8 sockets)
- Everything in Premium
- 24/7 Severity-1 response target (2 hours)
- Named technical contact
- Roadmap influence and design reviews
- Offline key activation when available
- Custom / multi-year quotes on request

Market context (Sep 2026): Proxmox VE Premium ~€1,100/socket/year (24/7, 2h Sev-1); Standard ~€550. Vates XCP-ng Enterprise ~$1,800/host/year.

## How pricing works

- Pricing is per physical CPU socket, not per core
- Each occupied socket on each subscribed node requires a subscription
- For production clusters, all nodes should be subscribed at the same level

## Migration & professional services

We are building a VMware migration work package. Until it ships, Tacconi Consulting Ltd is available for hire to plan and execute migrations from VMware and Proxmox onto kcore (assessment, pilot, cutover, Year-1 subscription). Early revenue is often services-led; recurring socket subscriptions follow (“subscriptions as the tail”).

Contact: [team@tacconiconsulting.com](mailto:team@tacconiconsulting.com?subject=kcore%20migration%20services).

## Partner program

We run a partner program for resellers, integrators, and service providers. Contact [team@tacconiconsulting.com](mailto:team@tacconiconsulting.com?subject=kcore%20partner%20program).

## FAQ

**Do I need a subscription to use kcore?**  
No. You can download Community Edition from GitHub Releases or build from source without a subscription. Subscriptions apply to the official supported distribution, the stable production update channel, signed ISOs, and commercial support.

**What does a subscription cover?**  
The official supported distribution: signed ISO images, access to the stable production update channel, tested updates, and support under the terms of Standard, Premium, or Enterprise.

**Is Community Edition open source?**  
Yes. The Community Edition core is licensed under the Apache License 2.0.

**Are subscriptions per core or per socket?**  
Per physical CPU socket, not per core.

**How do I buy?**  
Use Buy on [Pricing](https://kcorehypervisor.com/pricing.html). Stripe Payment Links can be configured; otherwise email team@tacconiconsulting.com for an invoice or checkout link.
