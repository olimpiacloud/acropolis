const express = require("express");
const cors = require("cors");
const helmet = require("helmet");
const morgan = require("morgan");
const { z } = require("zod");

const app = express();
app.use(cors());
app.use(helmet());
app.use(morgan("tiny"));
app.use(express.json());

const Item = z.object({ name: z.string().min(1), qty: z.number().int().positive() });
const items = [];

app.get("/", (_req, res) => res.json({ ok: true, service: "express-api", node: process.version }));
app.get("/items", (_req, res) => res.json(items));
app.post("/items", (req, res) => {
  const parsed = Item.safeParse(req.body);
  if (!parsed.success) return res.status(400).json(parsed.error.flatten());
  items.push(parsed.data);
  res.status(201).json(parsed.data);
});

const port = process.env.PORT || 3000;
app.listen(port, () => console.log(`listening on ${port}`));
