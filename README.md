## Installation

To install this application using Helm run the following commands: 

```bash
helm repo add jorritsalverda https://helm.jorritsalverda.com
kubectl create namespace jarvis-alpha-innotec-planner

helm upgrade \
  jarvis-alpha-innotec-planner \
  jorritsalverda/jarvis-alpha-innotec-planner \
  --install \
  --namespace jarvis-alpha-innotec-planner \
  --set secret.gcpServiceAccountKeyfile='{abc: blabla}' \
  --wait
```
