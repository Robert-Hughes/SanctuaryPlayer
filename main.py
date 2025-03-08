from copyreg import pickle
from email.headerregistry import UniqueUnstructuredHeader
from enum import unique
import flask
from flask import Flask, request, send_file
from google.cloud import datastore
from datetime import datetime
from datetime import timezone
import requests
import html
from html.parser import HTMLParser

app = Flask(__name__)

datastore_client = datastore.Client()

# Flask serves static files from the static/ subfolder. However our index.html
# isn't in that folder (deliberately, so that it can be easily hosted from GitHub Pages, for example),
# so we have to handle that manually here.
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
    video_release_date = request.args.get('video_release_date') # Optional
    if not user_id or not device_id or not video_id or not position:
        return ("Missing user_id or device_id or video_id or position in query args", 400)

    try:
        position = int(position)
    except ValueError:
        return ("position is not a valid int", 400)

    if video_release_date:
        try:
            video_release_date = datetime.fromisoformat(video_release_date)
        except ValueError:
            return ("video_release_date is not a valid ISO date", 400)

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
    if video_release_date:
        entity["video_release_date"] = video_release_date
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

# I can't find a way to get the title of a Twitch video from the player API and looking it up
# via a separate request to the Twitch API requires an auth token which can't be sent from the client side (otherwise it would leak our token!).
# Instead we do this server-side, and the web page will call this endpoint to get the title of a Twitch video.
# The same goes for video release dates, which we roll into the same API here.
@app.route("/get-video-metadata", methods=['GET'])
def handle_get_video_metadata():
    # Get parameters from request args
    video_id = request.args.get('video_id')
    video_platform = request.args.get('video_platform') # 'twitch' or 'youtube'
    if not video_id or not video_platform:
        return ("Missing video_id or video_platform in query args", 400)

    if video_platform == 'twitch':
        # We could use the Twitch API here, but that requires an auth token which will need refreshing etc.,
        # so it's simpler just to scrape it from the HTML page of the video.
        # Note it's important that we use https (rather than http), otherwise we get a redirect page that (sometimes?) doesn't include the title
        http_response = requests.get("https://www.twitch.tv/videos/" + video_id)
    elif video_platform == 'youtube':
        http_response = requests.get("https://www.youtube.com/v/" + video_id)
    else:
        return ("Invalid video_platform", 400)

    if http_response.status_code != 200:
        return ("Failed to fetch video page, status code " + str(http_response.status_code), 500)

    http_response.encoding = 'utf-8' # Even though the `requests` package is supposed to automatically detect encoding, it doesn't seem to work

    # Parse the web page and extract all the <meta> elements
    class MetaTagParser(HTMLParser):
        def __init__(self):
            super().__init__()
            self.metas = {}

        def handle_starttag(self, tag, attrs):
            if tag == 'meta':
                for (attr_key, attr_value) in attrs:
                    if attr_key in ('name', 'property', 'itemprop'):
                        meta_key = attr_value
                        for (attr_key2, attr_value2) in attrs:
                            if attr_key2 == 'content':
                                meta_content = attr_value2
                                self.metas[meta_key] = meta_content
                                return

    parser = MetaTagParser()
    parser.feed(http_response.text)

    video_title = parser.metas.get('og:title')
    video_release_date = parser.metas.get('og:video:release_date') or parser.metas.get('uploadDate') # Twitch and YouTube use different tags for this

    return flask.jsonify({ 'video_title': video_title, 'video_release_date': video_release_date })
