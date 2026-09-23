# syntax=docker/dockerfile:1.7

# A tiny stand-in mapper for local development and demos; see README.md in this directory and
# docs/mapper-api.md for what it implements and why it is not something to run in production.
# Python's standard library is enough, so there is nothing to install.
FROM python:3-slim
WORKDIR /app
COPY mapper.py catalog.json ./
EXPOSE 9911
CMD ["python3", "mapper.py"]
