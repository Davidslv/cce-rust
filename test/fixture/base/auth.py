import hashlib

def hash_password(password):
    return hashlib.sha256(password.encode()).hexdigest()

def verify_password(password, digest):
    return hash_password(password) == digest

class SessionManager:
    def create_session(self, user_id):
        return {"user": user_id}
