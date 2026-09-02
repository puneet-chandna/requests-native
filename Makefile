.PHONY: docs
init:
	python -m pip install -r requirements-dev.txt
test:
	python -m pytest tests

ci:
	python -m pytest tests --junitxml=report.xml

test-readme:
	python setup.py check --restructuredtext --strict && ([ $$? -eq 0 ] && echo "README.rst and HISTORY.rst ok") || echo "Invalid markup in README.rst or HISTORY.rst!"

coverage:
	python -m pytest --cov-config .coveragerc --verbose --cov-report term --cov-report xml --cov=src/requests tests

docs:
	cd docs && make html
	@echo "\033[95m\n\nBuild successful! View the docs homepage at docs/_build/html/index.html.\n\033[0m"
