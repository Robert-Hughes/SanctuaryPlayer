# Google Cloud Run will detect that our app is a python app (requirements.txt exists?) and then build a Docker container
# from it. This file defines the entrypoint to the webserver.
# gunicorn and its paramters are recommended here: https://cloud.google.com/run/docs/quickstarts/build-and-deploy/deploy-python-service
web: gunicorn --bind :$PORT --workers 1 --threads 8 --timeout 0 main:app