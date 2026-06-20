---
hide:
  - navigation
  - toc
---
![Selkies WebRTC](assets/logo/horizontal-480.png)

[![Build](https://github.com/selkies-project/selkies/actions/workflows/build_and_publish_all_images.yaml/badge.svg)](https://github.com/selkies-project/selkies/actions/workflows/build_and_publish_all_images.yaml)

[![Discord](https://img.shields.io/badge/dynamic/json?logo=discord&label=Discord%20Members&query=approximate_member_count&url=https%3A%2F%2Fdiscordapp.com%2Fapi%2Finvites%2FwDNGDeSW5F%3Fwith_counts%3Dtrue)](https://discord.gg/wDNGDeSW5F)

**Moonlight, Google Stadia, or GeForce NOW in noVNC form factor for Linux X11, in any HTML5 web interface you wish to embed inside, with at least 60 frames per second on Full HD resolution.**

**We are in need of maintainers and community contributors. Please consider stepping up, as we can never have too much help!**

Selkies is an open-source low-latency high-performance Linux-native GPU/CPU-accelerated HTML5 remote desktop streaming platform, for self-hosting, containers, Kubernetes, or Cloud/HPC platforms, [started out first by Google engineers](https://web.archive.org/web/20210310083658/https://cloud.google.com/solutions/gpu-accelerated-streaming-using-webrtc), then expanded by academic researchers. It streams over plain WebSockets by default, with WebRTC available as an opt-in transport.

Selkies is designed for researchers, including people in the graphical AI/robotics/autonomous driving/drug discovery fields, SLURM supercomputer/HPC system administrators, Jupyter/Kubernetes/Docker®/Coder infrastructure administrators, and Linux cloud gaming enthusiasts.

While designed for clustered or unprivileged containerized environments, Selkies can also be deployed in desktop computers, and any performance issue that would be problematic in cloud gaming platforms is also considered a bug.

**Please read [Troubleshooting and FAQs](faq.md) first, then use [Discord](https://discord.gg/wDNGDeSW5F) or [GitHub Discussions](https://github.com/selkies-project/selkies/discussions) for support questions. Please only use [Issues](https://github.com/selkies-project/selkies/issues) for technical inquiries or bug reports.**

**NOTE: this project is licensed under the [Mozilla Public License, version 2.0](https://www.mozilla.org/en-US/MPL/2.0/FAQ/), which obliges to share modified code files licensed by MPL-2.0 when distributed externally, but does not apply for any larger work outside this project, which might be open-source or proprietary under any license of choice. Externally originated components outside this project may contain works licensed over more restrictive copyleft/proprietary licenses, as well as other terms of intellectual property, including but not limited to patents, which users or developers are obliged to adhere to.**

[**What is Selkies?**](design.md)

[**Getting Started**](start.md)

[**Usage**](usage.md)

[**Troubleshooting and FAQs**](faq.md)

[**WebRTC and Firewall Issues (cannot connect)**](firewall.md)

[**Components including Encoders and Interfaces**](component.md)

[**Development and Contributions**](development.md)

## Citations in Academic Publications

> **NOTE: This section is also applicable for developers applying, embedding, forking, deriving, or taking influence from this project.**

**Citations are the currency of scientific research. Citing in your publications is the main driver of keeping this project alive.**

Remote desktop software projects have always been primarily proprietary. It is difficult to maintain such a project without consistent full-time compensation or financial upkeep.

Therefore, citations in academic publications are crucial for keeping this project under academic community governance, as well as to track and report the demographics of our users.

Therefore, we kindly, but strongly ask all software projects which are applying into, embedding in, forking from, deriving from, or taking influence from this project to retain the below sections in a clearly visible location of your project and/or documentation.

**Users of this open-source software project should cite the following publications when publishing in academic form to keep this project and original upstream projects sustainable:**

(Please note that this is currently a placeholder, an upcoming publication will be available after article review.)

`Kim, S., Isla, D., Hejtmánek, L., et al., Selkies-GStreamer, (2024), GitHub repository, https://github.com/selkies-project/selkies`

**Maintainers of derivative open-source projects should also place this text in a clearly visible location of your project.**

---
This project has been developed and is supported in part by the National Research Platform (NRP) and the Cognitive Hardware and Software Ecosystem Community Infrastructure (CHASE-CI) at the University of California, San Diego, by funding from the National Science Foundation (NSF), with awards #1730158, #1540112, #1541349, #1826967, #2138811, #2112167, #2100237, and #2120019, as well as additional funding from community partners, infrastructure utilization from the Open Science Grid Consortium, supported by the National Science Foundation (NSF) awards #1836650 and #2030508, and infrastructure utilization from the Chameleon testbed, supported by the National Science Foundation (NSF) awards #1419152, #1743354, and #2027170. This project has also been funded by the Seok-San Yonsei Medical Scientist Training Program (MSTP) Song Yong-Sang Scholarship, College of Medicine, Yonsei University, the MD-PhD/Medical Scientist Training Program (MSTP) through the Korea Health Industry Development Institute (KHIDI), funded by the Ministry of Health & Welfare, Republic of Korea, and the Student Research Bursary of Song-dang Institute for Cancer Research, College of Medicine, Yonsei University.

<sub><sup>\* Funding agencies including, but not limited to the National Science Foundation, remain neutral with regard to jurisdictional claims in published articles and software code of this Code Repository. The Selkies Project logo and name have been created and are utilized or distributed with authorization by Dan Isla. In the context including, but not limited to this Code Repository, as well as in the context including, but not limited to any and all derivative works based on this Code Repository, all trademarks, trade names, logos, patents, or any and all other forms of external intellectual property, that are mentioned or used, unless otherwise stated, are the property of their respective owners, including but not limited to, The Linux Foundation®, Linus Torvalds, The Apache Software Foundation, Canonical Ltd., Google LLC, Alphabet Inc., NumFOCUS Foundation, Anaconda Inc., conda-forge, Project Jupyter, Coder Technologies, Inc., Docker®, Inc., SchedMD LLC, NVIDIA Corporation, Intel Corporation, Advanced Micro Devices, Inc., Valve Corporation, Epic Games, Inc., Unity Software Inc., Cendio AB, RealVNC® Limited, Amazon.com, Inc., Amazon Web Services, Inc., or its affiliates including but not limited to NICE s.r.l. or NICE USA LLC, Microsoft Corporation, Cloudflare, Inc., Oracle Corporation, StarNet Communications Corporation, TeamViewer SE, GStreamer Foundation, Fabrice Bellard, Moonlight Project, and LizardByte. Every best effort has been undertaken to properly identify and attribute trademarks, trade names, logos, patents, or any and all other forms of external intellectual property to their respective owners, unless otherwise stated, wherever possible and practical. The inclusion of such trademarks, trade names, logos, patents, or any and all other forms of external intellectual property in association with this project, unless otherwise stated, serves solely for the purpose of description and must never be construed as an indication of affiliation, competition, endorsement, or a challenge to any and all legal standings of the trademarks, trade names, logos, patents, or any and all other forms of external intellectual property. All project contributors, maintainers, owners, or organizations agree to not willfully breach or infringe legal regulations, in any and all global law, regarding trademarks, trade names, logos, patents, or any and all other forms of external intellectual property. Therefore, all project contributors, maintainers, owners, or organizations, are immune to, and are not to be in any and all cases held legally liable for, any and all jurisdictional claims on trademarks, trade names, logos, patents, or any and all other forms of external intellectual property. No component of this Code Repository is an official product of Google LLC or Alphabet Inc.</sup></sub>
