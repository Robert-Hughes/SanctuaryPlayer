# Note that the name of this file is important, as Google App Engine will look for a file called main.py
# containing a WSGI-compatible object by default - https://cloud.google.com/appengine/docs/standard/python3/runtime#application_startup

# Note that static files are not served from Flask, as that isn't recommended: 
# Docs: https://flask.palletsprojects.com/en/2.1.x/quickstart/#static-files
# https://cloud.google.com/appengine/docs/standard/python3/serving-static-files
# See also example: https://cloud.google.com/appengine/docs/standard/python3/building-app/writing-web-service
# Instead we configure this in the app.yaml so that Google App Engine serves them directly, without ever
# going to our Flask server.

from copyreg import pickle
from email.headerregistry import UniqueUnstructuredHeader
from enum import unique
from flask import Flask, request, send_file
from google.cloud import datastore
from datetime import datetime
from datetime import timezone

app = Flask(__name__)

datastore_client = datastore.Client()

# Even though we don't use Flask to serve static files (including index.html), when debugging locally
# Flash _does_ serve static files for us, normally from the static/ subfolder. However our index.html
# isn't in that folder (deliberately, so that it can be easily hosted from GitHub Pages, for example),
# so we have to handle that manually here. On the production Google App Engine server, this request
# should never reach Flask, because Google will serve the index.html itself based on the rules in our
# app.yaml.
@app.route('/')
def serve_index():
    return send_file("index.html")

@app.route("/save-position", methods=['POST'])
def handle_save_position():   
    # Get parameters from request args
    user_id = request.args.get('user_id')
    device_id = request.args.get('device_id')
    video_id = request.args.get('video_id')
    position = request.args.get('position')
    video_title = request.args.get('video_title') # Optional
    if not user_id or not device_id or not video_id or not position:
        return ("Missing user_id or device_id or video_id or position in query args", 400)
    try:
        position = float(position)
    except ValueError:
        return ("position is not a valid floating point number", 400)

    # Current timestamp
    modified_time = datetime.now(timezone.utc) # Timezone-aware with timezone set to UTC, so that it plays nicely with Google Datastore

    # Construct Key to uniquely identify the Entity in the datastore database. If the user already has a saved position
    # for this video from the same device, it will overwrite.
    # Note that this simple key structure with slashes could technically be exploited (different tuples could make to the same key), but that's fine for now.
    entity = datastore_client.entity(key = datastore_client.key('SavedPosition', user_id + '/' + device_id + '/' + video_id))
    entity['user_id'] = user_id
    entity['device_id'] = device_id
    entity['video_id'] = video_id
    entity["modified_time"] = modified_time
    entity["position"] = position
    entity["video_title"] = video_title
    datastore_client.put(entity)

    return "OK"

@app.route("/get-saved-positions", methods=['GET'])
def handle_get_saved_positions():
    # Get parameters from request args
    user_id = request.args.get('user_id')
    device_id = request.args.get('device_id')
    if not user_id or not device_id:
        return ("Missing user_id or device_id in query args", 400)
    # Video ID is optional
    current_video_id = request.args.get('current_video_id')
    
    def to_json(entities):
        result = []
        for r in entities:
            device_id = r['device_id']
            video_id = r['video_id']
            position = r['position']
            modified_time = r['modified_time']
            video_title = r.get('video_title', '') # Older enitities won't have title
            result.append({ 'device_id': device_id, 'video_id': video_id, 'position': position, 'modified_time': modified_time, 'video_title': video_title })
        return result

    # Find the most recent saved positions for other videos for this user, across all their devices.
    # This basically shows what you were last watching on each device, independent of what you're watching now.
    # We can't exclude the current videos using this query (limitation of Datastore), so we do it manually once we get the results.
    query = datastore_client.query(kind='SavedPosition')
    query.add_filter('user_id', '=', user_id)
    query.order = ['device_id', '-modified_time']
    query.distinct_on = ['device_id']
    query_results = list(query.fetch())
    if current_video_id:
        query_results = [r for r in query_results if r['video_id'] != current_video_id]

    json = { 'other_videos': to_json(query_results) }
            
    # Additionally, if a current_video_id was provided, show all the saved positions for this video on other devices.
    # This shows where you were up to on this video on different devices, even if it wasn't the most recent video you watched that device.
    # Note that we exclude the current device, because the saved position on the current device will be the position the user is already at,
    # so no point showing them that!
    # We can't exclude the current device using this query (limitation of Datastore), so we do it manually once we get the results.
    if current_video_id:
        query = datastore_client.query(kind='SavedPosition')
        query.add_filter('user_id', '=', user_id)
        query.add_filter('video_id', '=', current_video_id)
        query.order = ['-modified_time']
        query_results = list(query.fetch())
        query_results = [r for r in query_results if r['device_id'] != device_id]

        json['this_video'] = to_json(query_results)           

    return json
