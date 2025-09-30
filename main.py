from copyreg import pickle
from email.headerregistry import UniqueUnstructuredHeader
from enum import unique
from time import sleep
import flask
from flask import Flask, request, send_file
from google.cloud import datastore
from datetime import datetime, timezone, timedelta
import requests
import html
from html.parser import HTMLParser

app = Flask(__name__)

# Configure the JSON serializer to handle datetime objects by converting them to an ISO string (e.g. 2025-03-04T12:53:27Z) so that
# they can be easily parsed by the client side.
class CustomJSONProvider(flask.json.provider.DefaultJSONProvider):
    def default(self, o):
        if isinstance(o, datetime):
            return o.isoformat()
        return super().default(self, o)

app.json = CustomJSONProvider(app)

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
    if not user_id or not device_id or not video_id or not position:
        return ("Missing user_id or device_id or video_id or position in query args", 400)

    try:
        position = int(position)
    except ValueError:
        return ("position is not a valid int", 400)

    # Current timestamp
    modified_time = datetime.now(timezone.utc) # Timezone-aware with timezone set to UTC, so that it plays nicely with Google Datastore

    # Construct Key to uniquely identify the Entity in the datastore database. If the user already has a saved position
    # for this video from the same device, it will overwrite.
    # Note that this simple key structure with slashes could technically be exploited (different tuples could make to the same key), but that's fine for now.
    key = datastore_client.key('SavedPosition', user_id + '/' + device_id + '/' + video_id)
    entity = datastore_client.entity(key)
    entity['user_id'] = user_id
    entity['device_id'] = device_id
    entity['video_id'] = video_id
    entity["modified_time"] = modified_time
    entity["position"] = position
    # The 'expiry_date' field is configured in our Datastore TTL Policy to delete entities after this time passes, which we use
    # to avoid filling up with old and (most likely) unused data.
    entity["expiry_date"] = datetime.now(timezone.utc) + timedelta(days=365)

    datastore_client.put(entity)

    return "OK"


@app.route("/get-saved-positions", methods=['GET'])
def handle_get_saved_positions():
    # Get parameters from request args
    user_id = request.args.get('user_id')
    if not user_id:
        return ("Missing user_id in query args", 400)

    # Find the most recent saved positions for this user, across all their devices.
    # This will include positions for the current video (if there is one), and positions for other videos.
    query = datastore_client.query(kind='SavedPosition')
    query.add_filter(filter=datastore.query.PropertyFilter('user_id', '=', user_id))
    query.order = ['-modified_time']
    saved_positions = list(query.fetch(limit=10)) # Limit the number of results as there might be a lot
    # Convert from entities to dictionaries
    saved_positions = list(map(lambda e: dict(e), saved_positions))

    # Lookup the corresponding metadata for the videos, which we should have in our database too
    # (as it would have been queried by the app when first playing the video).
    # This is so that we can display the video titles and release dates in the app.
    video_metadata_keys = set([datastore_client.key('VideoMetadata', saved_position['video_id']) for saved_position in saved_positions])
    video_metadatas = datastore_client.get_multi(video_metadata_keys)
    # Convert from list to dictionary for easy lookup
    video_metadatas = {video_metadata.key.name: video_metadata for video_metadata in video_metadatas}

    # Perform a 'manual join' of the video metadata into the saved_positions dicts
    for saved_position in saved_positions:
        video_id = saved_position['video_id']
        video_metadata = video_metadatas.get(video_id)
        if video_metadata: # It's possible that we don't have metadata for this video, so just don't add those properties
            saved_position['video_title'] = video_metadata['video_title']
            saved_position['video_release_date'] = video_metadata['video_release_date']

    return flask.jsonify(saved_positions)

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

    # We may have already stored metadata for this video, in which case we don't need to send any HTTP requests
    key = datastore_client.key('VideoMetadata', video_id)
    existing = datastore_client.get(key)
    if existing:
        return flask.jsonify(existing)

    # May need to try multiple times (see below)
    for retry_count in range(5):
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

        # When fetching the content of a Twitch or YouTube video page, sometimes it doesn't get the full page but instead a minimal/shell version that contains just
        # a loading icon, and then some javascript code which presumably loads the actual content in the background. This is presumably to make the page load fast
        # and seem responsive, but is annoying for us which need the full page. We try to detect this and reload the page, which tends to get the full one.
        if not 'og:video' in http_response.text:
            print("Trying again to get full page...")
            sleep(1) # Wait a bit before retrying
            continue

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
        video_release_date = datetime.fromisoformat(video_release_date) # Convert from string into datetime object, so that it's stored better in the database

        break # Success!

    # Store the video metadata in our database so that when displaying a list of saved positions in the app,
    # we can include useful information.
    entity = datastore_client.entity(key)
    entity["video_title"] = video_title
    entity["video_release_date"] = video_release_date
    # The 'expiry_date' field is configured in our Datastore TTL Policy to delete entities after this time passes, which we use
    # to avoid filling up with old and (most likely) unused data.
    entity["expiry_date"] = datetime.now(timezone.utc) + timedelta(days=365)
    datastore_client.put(entity)

    return flask.jsonify(entity)
