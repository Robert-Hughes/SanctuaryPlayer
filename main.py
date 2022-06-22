# Note that the name of this file is important, as Google App Engine will look for a file called main.py
# containing a WSGI-compatible object by default - https://cloud.google.com/appengine/docs/standard/python3/runtime#application_startup

from flask import Flask, request
from google.cloud import datastore

app = Flask(__name__)

datastore_client = datastore.Client()

@app.route("/save-position", methods=['POST'])
def test():
    return "Hello test!" + str(request.args)

# Note that static files are not served from Flask, as that isn't recommended: 
# Docs: https://flask.palletsprojects.com/en/2.1.x/quickstart/#static-files
# https://cloud.google.com/appengine/docs/standard/python3/serving-static-files
# See also example: https://cloud.google.com/appengine/docs/standard/python3/building-app/writing-web-service
# Instead we configure this in the app.yaml so that Google App Engine serves them directly, without ever
# going to our Flask server.