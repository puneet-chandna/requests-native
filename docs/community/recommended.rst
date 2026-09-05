.. Requests Native modification notice: this retained file differs from Requests 2.34.2.

.. _recommended:

Recommended Packages and Extensions
===================================

The packages below are part of the broader Requests ecosystem. They are listed
for API compatibility context, not as independently verified Requests Native
integrations.

Certifi CA Bundle
-----------------

`Certifi`_ is a carefully curated collection of Root Certificates for
validating the trustworthiness of SSL certificates while verifying the
identity of TLS hosts. It has been extracted from the Requests project.

.. _Certifi: https://github.com/certifi/python-certifi

CacheControl
------------

`CacheControl`_ is an extension that adds a full HTTP cache to Requests. This
makes your web requests substantially more efficient, and should be used
whenever you're making a lot of web requests.

.. _CacheControl: https://cachecontrol.readthedocs.io/en/latest/

Requests-Toolbelt
-----------------

`Requests-Toolbelt`_ is a collection of utilities used with Requests. Its
compatibility with Requests Native depends on the extension behavior it uses;
report rewrite-specific differences with a minimal upstream comparison.

.. _Requests-Toolbelt: https://toolbelt.readthedocs.io/en/latest/index.html


Requests-Threads
----------------

`Requests-Threads`_ is a Requests session that returns the amazing Twisted's awaitable Deferreds instead of Response objects. This allows the use of ``async``/``await`` keyword usage on Python 3, or Twisted's style of programming, if desired.

.. _Requests-Threads: https://github.com/requests/requests-threads

Requests-OAuthlib
-----------------

`requests-oauthlib`_ makes it possible to do the OAuth dance from Requests
automatically. This is useful for the large number of websites that use OAuth
to provide authentication. It also provides a lot of tweaks that handle ways
that specific OAuth providers differ from the standard specifications.

.. _requests-oauthlib: https://requests-oauthlib.readthedocs.io/en/latest/


Betamax
-------

`Betamax`_ records your HTTP interactions so the NSA does not have to.
A VCR imitation designed only for Python-Requests.

.. _betamax: https://github.com/betamaxpy/betamax
