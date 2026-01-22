## Purpose

This is a simple project with the goal of syncing files across a few people using google cloud buckets.

Was used to sync songs for Clone Hero, between a small group. 

It uploads and zips files to the bucket. And when pulling it grabs the zipped files and proceeds to unzip.

## Setup

Create a gcp-key.json from google cloud, and paste into the main directory

Fill in config.ini 
  Target Dir - The place where files will be downloaded to
  Uplaod Dir - The place where files where they will be uploaded from (this will more than likely be the same as target dir)
  Max Size - The maximum file size in mB
  
  Project Id - This was never used but I left it in
  Bucket Name - The name of the google bucket

The program should run fine from here, use the instructions displayed to sync to the bucket!
