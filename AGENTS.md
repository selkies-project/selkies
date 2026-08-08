# Working on this repository

Selkies is developed together with two sibling repositories, [pixelflux](https://github.com/selkies-project/pixelflux)
(screen capture and video encode) and [pcmflux](https://github.com/selkies-project/pcmflux) (audio capture and
encode). A change in one often belongs in another; coordinate across all three.

Use web search, web fetch, and other available tools as necessary. Make sure that the comments or documentation are
not too verbose (do not add comments more fit for a PR summary than a comment). Do not leave arbitrary numbers (such
as issue or task numbers) in the code or documentation. Do not use inline comments. Do not use comments or
documentation that describe arbitrary code changes of previous states compared to the current code that do not need
explanation. The code commenting should reflect the current state of the codebase and be used to convey information
to an LLM bot or developer.

Empirical testing is a very useful way to develop this project, and empirical testing is possible for EVERYTHING,
including implementation, auditing, validation, or verification. A few such options are by utilizing the currently
installed Firefox and Chrome, as well as the WebKit engine provided by Playwright/Selenium/Puppeteer/Cypress in place
of Safari, for end-to-end tests. HOWEVER, ask the user for permission to create a test environment (possibly using
Miniforge; but note that it is likely the system `libgbm.so` should be used for GBM support on NVIDIA and other GPUs)
and receive directives from the user on how the environment should be constructed and constrained.

Note that parity between X11 and Wayland, as well as between WebSockets and WebRTC, or between the default dashboard
and the wish dashboard, is considered a key focus (things that were not wired up correctly on either side, and similar
discrepancies, are subject to fixes or deduplication). I prefer deduplicating code that performs similar purposes
across different modes over keeping duplicate code for no reason and more fragility. Refactor through deduplication if
you are confident there will be no regressions (or able to validate regressions). Screen coroutine usage in both
Python and JavaScript, as well as thread usage in all languages, so that everything is performant and does not lead to
hanging or lagging. Performance preservation or improvements such as zero-copy and latency-reducing measures are
always important. Note that compatibility should be ensured for Python 3.9 to 3.14 or even higher, and CUDA/NVENC 11
to 13 or higher. Update the translations as well (and write/update additional entries if necessary) as necessary.
Update this file when certain details change.
